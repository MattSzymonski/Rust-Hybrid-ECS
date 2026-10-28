//! The scene and game viewports: their geometry and overlay chips.
//!
//! # Responsibilities
//!
//! - The measured scene body rectangle the frontend places the scene's child
//!   window at, and the game view's own rectangle.
//! - The per-viewport overlay chip (the frame counter) and the region it marks.
//!
//! The globs below are absolute paths: a generated patch crate replays this
//! file's top-level `use` lines, so a `super::`-relative glob would not
//! resolve there.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// How far the scene rectangle extends past the measured tab body, in
/// logical points: the body's padding and outline sit inside the leaf, and
/// the child window must back them too, so no sliver of the game view
/// behind the overlay shows around the viewport's edge.
pub const SCENE_RECT_OVERSHOOT: f32 = 16.0;

/// A shell update slower than this logs its own `[editor] egui frame` line
/// in addition to the 210-frame heartbeat. The bar is a whole 60 Hz frame: a
/// debug build's pass runs several milliseconds, so a lower one would log a
/// line per frame. A pass that eats a whole frame is worth its own record.
pub const SLOW_UPDATE_MICROS: u64 = 16_000;

/// The scene's frame counter: its box in logical points, and the gap to the
/// tab body's corner.
pub const SCENE_COUNTER_WIDTH: f32 = 76.0;

pub const SCENE_COUNTER_HEIGHT: f32 = 20.0;

pub const SCENE_COUNTER_MARGIN: f32 = 8.0;

/// Convert an egui rectangle into the boundary's plain rectangle.
pub fn shell_rect(rect: egui::Rect) -> ShellRect {
    ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    }
}

/// A scene tab's body rectangle in logical points, overshooting by
/// [`SCENE_RECT_OVERSHOOT`] so the child window backs the tab body's padding
/// and outline too.
pub fn scene_body_rect(ui: &egui::Ui) -> ShellRect {
    shell_rect(ui.max_rect().expand(SCENE_RECT_OVERSHOOT))
}

/// A scene tab's overlay strip: the frame counter, anchored to the viewport's
/// top-right corner and painted from the theme. The overlay paints above the
/// scene's child window, so the counter sits over the viewport, and it takes
/// input, so a click on it never reaches the project. Its rectangle joins the
/// frame's overlay regions, so a click on it is never read as a scene pick.
pub fn scene_overlay(
    ui: &mut egui::Ui,
    frames_per_second: f32,
    overlay_regions: &mut Vec<ShellRect>,
) {
    let counter_rect = frame_counter_rect(ui);
    overlay_regions.push(shell_rect(counter_rect));
    // The counter is painted rather than laid out, so it costs the tab body
    // no layout space; its rectangle still senses clicks.
    let _ = ui.interact(
        counter_rect,
        ui.id().with("scene_frame_counter"),
        egui::Sense::click(),
    );
    ui.painter().rect(
        counter_rect,
        egui::CornerRadius::same(5),
        style::overlay(),
        egui::Stroke::new(1.0, style::overlay_border()),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        counter_rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{frames_per_second:.0} fps"),
        egui::TextStyle::Monospace.resolve(ui.style()),
        style::text_strong(),
    );
}

/// The frame counter's rectangle: the strip's right-anchored box.
pub fn frame_counter_rect(ui: &egui::Ui) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            ui.max_rect().right() - SCENE_COUNTER_WIDTH - SCENE_COUNTER_MARGIN,
            ui.max_rect().top() + SCENE_COUNTER_MARGIN,
        ),
        egui::vec2(SCENE_COUNTER_WIDTH, SCENE_COUNTER_HEIGHT),
    )
}

/// The padding a badge's mark leaves inside its clickable rectangle, in
/// points; the icon size preference sets the mark, and this is the rest.
pub const SCENE_BADGE_PADDING: f32 = 3.0;

/// The spread a badge's shadow is blurred over, as a fraction of the mark's
/// size.
pub const SCENE_BADGE_SHADOW_BLUR: f32 = 0.1;

/// How far the whole shadow sits down and right of the mark, as a fraction of
/// the mark's size: a CSS shadow's direction, kept short so the mark still
/// reads as standing on its point.
pub const SCENE_BADGE_SHADOW_DISTANCE: f32 = 0.05;

/// The opacity of one shadow sample. The shadow is the mark drawn at
/// [`SCENE_BADGE_SHADOW_SAMPLES`], so the faded copies add up in the middle and
/// thin out at the rim.
pub const SCENE_BADGE_SHADOW_ALPHA: f32 = 0.22;

/// The unit offsets a badge's shadow is sampled at: the centre and a ring
/// around it, which together read as one soft edge instead of a second hard
/// glyph.
pub const SCENE_BADGE_SHADOW_SAMPLES: [(f32, f32); 9] = [
    (0.0, 0.0),
    (1.0, 0.0),
    (0.5, 0.5),
    (0.0, 1.0),
    (-0.5, 0.5),
    (-1.0, 0.0),
    (-0.5, -0.5),
    (0.0, -1.0),
    (0.5, -0.5),
];

/// The centres a badge's shadow is drawn at: the mark's own centre moved to the
/// shadow's place, then to each blur sample around it.
pub fn badge_shadow_centres(mark: egui::Rect) -> [egui::Pos2; SCENE_BADGE_SHADOW_SAMPLES.len()] {
    let distance = mark.width() * SCENE_BADGE_SHADOW_DISTANCE;
    let blur = mark.width() * SCENE_BADGE_SHADOW_BLUR;
    let origin = mark.center() + egui::vec2(distance, distance);
    let mut centres = [origin; SCENE_BADGE_SHADOW_SAMPLES.len()];
    for (centre, (x, y)) in centres.iter_mut().zip(SCENE_BADGE_SHADOW_SAMPLES) {
        *centre = origin + egui::vec2(x * blur, y * blur);
    }
    centres
}

/// Draw one scene view's entity badges: the entity's component mark - a
/// camera, a light - or a plain dot when it has neither, at each point the
/// executable projected.
///
/// A badge is the mark alone, with no chip behind it, standing on a short
/// blurred drop shadow in the theme's shadow ink - the way a CSS shadow keeps a
/// glyph readable over a busy surface. The mark's size is the
/// `editor.gizmo_icon_size` preference, and the rectangle that picks it scales
/// with it. The mark carries the state: a selected entity's is red, and
/// hovering brightens an unselected one. A badge is the UI's: its rectangle
/// joins the frame's overlay regions, so a click on it selects its entity
/// instead of pressing the scene behind it.
pub fn scene_badges(viewer: &mut EditorTabViewer<'_>, ui: &mut egui::Ui, view: usize) {
    let badges: Vec<ShellBadge> = viewer
        .badges
        .iter()
        .filter(|badge| badge.view == view)
        .copied()
        .collect();
    if badges.is_empty() {
        return;
    }
    // The preference is parsed and clamped into its range by the snapshot, so
    // this only has to size the mark and the rectangle around it.
    let icon_size = viewer.snapshot.preferences.gizmo_icon_size;
    let badge_size = icon_size + SCENE_BADGE_PADDING * 2.0;
    let mut picked = None;
    for badge in badges {
        let rect = egui::Rect::from_center_size(
            egui::pos2(badge.x, badge.y),
            egui::Vec2::splat(badge_size),
        );
        let response = ui.interact(
            rect,
            egui::Id::new(("pill_editor_shell_egui.scene_badge", badge.entity)),
            egui::Sense::click(),
        );
        let hovered = response.hovered();
        let selected = viewer.selection.entities().contains(&badge.entity);
        let key = format!("{view}:{}", badge.entity);
        if remember_moved_rect(&mut viewer.inspector.badge_rects, &key, rect) {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                view = badge.view,
                entity = %badge.entity,
                min_x = rect.min.x,
                min_y = rect.min.y,
                max_x = rect.max.x,
                max_y = rect.max.y,
                "[editor] scene badge rect"
            );
        }
        let label = viewer
            .snapshot
            .entities
            .iter()
            .find(|entry| entry.entity == badge.entity)
            .map(|entry| entry.archetype_label.as_str())
            .unwrap_or_default();
        // The shadow keeps the theme's shadow ink whatever the mark is doing:
        // hover and selection are read from the mark's own colour.
        let shadow_ink = icons::with_alpha(style::shadow(), SCENE_BADGE_SHADOW_ALPHA);
        let mark = rect.shrink(SCENE_BADGE_PADDING);
        let ink = match (selected, hovered) {
            (true, _) => style::danger(),
            (false, true) => style::text_strong(),
            (false, false) => style::text(),
        };
        match inspector::badge_icon(label) {
            Some(icon) => {
                // A blurred shadow is the mark itself drawn at each sample of
                // [`badge_shadow_centres`], so the copies sum into a soft edge.
                for centre in badge_shadow_centres(mark) {
                    icons::paint_duotone(
                        ui.painter(),
                        egui::Rect::from_center_size(centre, mark.size()),
                        icon,
                        shadow_ink,
                    );
                }
                icons::paint_duotone(ui.painter(), mark, icon, ink);
            }
            None => {
                let dot_ink = match (selected, hovered) {
                    (true, _) => style::danger(),
                    (false, true) => style::text_strong(),
                    (false, false) => style::text_muted(),
                };
                for centre in badge_shadow_centres(mark) {
                    ui.painter()
                        .circle_filled(centre, icon_size * 0.3, shadow_ink);
                }
                ui.painter()
                    .circle_filled(mark.center(), icon_size * 0.3, dot_ink);
            }
        }
        // A hovered badge used to outline itself in the accent. Paused: the
        // mark brightens instead, so no box is put back around it.
        //
        // if hovered {
        //     ui.painter().rect_stroke(
        //         rect,
        //         egui::CornerRadius::same(3),
        //         egui::Stroke::new(1.0, style::accent()),
        //         egui::StrokeKind::Inside,
        //     );
        // }
        if response.clicked() {
            picked = Some(badge.entity);
        }
        viewer.overlay_regions.push(shell_rect(rect));
    }
    if let Some(entity) = picked {
        select_entity(viewer.selection, entity);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            entity = %entity,
            "[editor] scene badge"
        );
    }
}

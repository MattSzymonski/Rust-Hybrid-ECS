//! The brutalist chunky button: a face with a hard offset shadow that presses
//! into it.
//!
//! # Responsibilities
//!
//! - [`ChunkyButton`]: a button whose face sits on a shadow of its own, drawn
//!   from a [`Theme`](pill_ui_theme::Theme)'s tokens, in three variants -
//!   neutral, accent and selected - plus a disabled face.
//! - [`ChunkyButton::new`]: the text and the theme; `.accent(..)` and
//!   `.selected(..)` pick the variant.
//!
//! # Design
//!
//! Stock egui cannot draw the look that defines the brutalist recipe: a hard
//! offset shadow, no blur, that the face drops onto while the button is held.
//! `Button::fill` freezes every state (`research §6`), so this widget paints
//! its own face per state instead of dressing a stock button: the rectangle is
//! allocated first, the state read from the response, then the shadow and the
//! face painted - and nothing painted at all when the rectangle is clipped.
//!
//! The widget stores nothing in egui's memory. A project's drawing code is a
//! reloadable image, and a value left in a context's memory could outlive it;
//! everything it needs comes from the `Theme` the caller passes and the
//! `Response` it returns.

// External crates
use egui::{
    Align2, Color32, FontId, Rect, Response, Sense, Shape, Stroke, StrokeKind, TextStyle,
    TextWrapMode, Ui, WidgetInfo, WidgetText, WidgetType,
};

// Current crate
use pill_ui_theme::{Rgba, Theme};

/// How far the shadow sits from the face, in points.
const SHADOW_OFFSET: f32 = 3.0;

/// The outline's width, in points.
const OUTLINE_WIDTH: f32 = 2.0;

/// The chosen face's selection bar, in points.
const SELECTION_BAR: f32 = 4.0;

/// A button with a hard offset shadow, drawn from a theme's tokens.
///
/// The response is a plain button's: a click is a press and a release on the
/// same widget, and `add_enabled(false, ..)` answers no click at all.
pub struct ChunkyButton<'a> {
    /// The label the face shows.
    text: WidgetText,
    /// The tokens the face is painted with.
    theme: &'a Theme,
    /// Whether the face is the accent one.
    accent: bool,
    /// Whether the face is the accent one and reads as chosen.
    selected: bool,
}

impl<'a> ChunkyButton<'a> {
    /// A neutral chunky button labelled `text`, painted from `theme`.
    pub fn new(text: impl Into<WidgetText>, theme: &'a Theme) -> Self {
        Self {
            text: text.into(),
            theme,
            accent: false,
            selected: false,
        }
    }

    /// Use the accent face: the accent fill, the frame's ink on it, and the hot
    /// accent while hovered or held.
    pub fn accent(mut self, accent: bool) -> Self {
        self.accent = accent;
        self
    }

    /// Mark the button as the chosen one of its group: the accent face, plus a
    /// bar of the selection colour beside the shadow.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// The face's fill and ink for the state the response reports.
    ///
    /// Every colour is a token: the accent face is the accent or its hot twin,
    /// the neutral one steps raised to control to line as the button is
    /// hovered and held, and a disabled face falls back to the panel.
    fn face(&self, enabled: bool, hovered: bool, held: bool) -> (Color32, Color32) {
        let palette = &self.theme.palette;
        if !enabled {
            return (color(palette.panel), color(palette.text_muted));
        }
        if self.accent || self.selected {
            let face = if hovered || held {
                palette.accent_hot
            } else {
                palette.accent
            };
            (color(face), color(palette.frame))
        } else {
            let face = if held {
                palette.line
            } else if hovered {
                palette.control
            } else {
                palette.panel_raised
            };
            (color(face), color(palette.text))
        }
    }
}

impl egui::Widget for ChunkyButton<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        // Step 1: the layout. The shadow's offset is part of the size, so the
        // face never overlaps what comes next.
        let galley = self.text.clone().into_galley(
            ui,
            Some(TextWrapMode::Extend),
            f32::INFINITY,
            TextStyle::Button,
        );
        let padding = egui::vec2(
            self.theme.spacing.button_padding[0] + 4.0,
            self.theme.spacing.button_padding[1] + 3.0,
        );
        let size = galley.size() + padding * 2.0 + egui::vec2(SHADOW_OFFSET, SHADOW_OFFSET);
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        let label = galley.text().to_owned();
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), &label));

        // Step 2: the paint, when the rectangle is on screen at all. A clipped
        // button draws nothing - not a shadow, not an outline.
        if ui.is_rect_visible(rect) {
            let radius = egui::CornerRadius::same(self.theme.shape.corner_radius);
            let held = response.is_pointer_button_down_on();
            let (fill, ink) = self.face(ui.is_enabled(), response.hovered(), held);
            // Held, the face sits where the shadow was: the button reads as
            // pressed into the surface rather than as merely coloured.
            let drop = if held { 0.0 } else { SHADOW_OFFSET };
            let face_rect = rect.shrink(SHADOW_OFFSET);
            let shadow_rect = face_rect.translate(egui::vec2(drop, drop));
            let painter = ui.painter();
            painter.add(Shape::rect_filled(
                shadow_rect,
                radius,
                color(self.theme.palette.line),
            ));
            if self.selected {
                let bar = Rect::from_min_size(
                    shadow_rect.left_top(),
                    egui::vec2(SELECTION_BAR, shadow_rect.height()),
                );
                painter.add(Shape::rect_filled(
                    bar,
                    radius,
                    color(self.theme.palette.selection_bar),
                ));
            }
            painter.add(Shape::rect_filled(face_rect, radius, fill));
            painter.add(Shape::rect_stroke(
                face_rect,
                radius,
                Stroke::new(OUTLINE_WIDTH, color(self.theme.palette.line)),
                StrokeKind::Inside,
            ));
            let size = self
                .theme
                .text_styles
                .get("button")
                .map_or(12.0, |style| style.size);
            let family = ui.style().text_styles[&TextStyle::Button].family.clone();
            painter.text(
                face_rect.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::new(size, family),
                ink,
            );
        }
        response
    }
}

/// An egui colour from a theme colour (straight sRGB alpha).
fn color(value: Rgba) -> Color32 {
    let [red, green, blue, alpha] = value.0;
    Color32::from_rgba_unmultiplied(red, green, blue, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, PointerButton, Pos2, RawInput};

    /// The theme the tests paint with.
    fn theme() -> Theme {
        Theme::brutalist()
    }

    /// The screen every pass draws on.
    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, egui::vec2(240.0, 160.0))
    }

    /// The filled rectangles a pass painted, as (rect, fill).
    ///
    /// The full-screen background a pass may paint is not a face, so it is
    /// left out; the button's own rectangles are the rest.
    fn faces(output: &egui::FullOutput) -> Vec<(Rect, Color32)> {
        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                Shape::Rect(rect) if rect.stroke.width == 0.0 => Some((rect.rect, rect.fill)),
                _ => None,
            })
            .filter(|(rect, _)| rect.width() < screen().width())
            .collect()
    }

    /// The button face among the painted rectangles: the widest one.
    fn widest(painted: &[(Rect, Color32)]) -> Option<Color32> {
        painted
            .iter()
            .max_by(|left, right| left.0.width().total_cmp(&right.0.width()))
            .map(|(_, fill)| *fill)
    }

    /// One pass on `context` with `events`, running `build`.
    ///
    /// The output's texture deltas are cleared before it is returned: epaint
    /// panics on a delta dropped with unapplied content, and a test applies
    /// none.
    fn pass(
        context: &egui::Context,
        events: Vec<Event>,
        index: usize,
        mut build: impl FnMut(&mut Ui) -> Response,
    ) -> egui::FullOutput {
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(screen()),
                events,
                time: Some(index as f64 * 0.1),
                ..Default::default()
            },
            |ui| {
                build(ui);
            },
        );
        output.textures_delta.clear();
        output
    }

    /// A press and a release at `center`.
    fn press_at(center: Pos2) -> Vec<Vec<Event>> {
        vec![
            vec![Event::PointerMoved(center)],
            vec![Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            vec![Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        ]
    }

    #[test]
    fn a_press_and_a_release_make_a_click() {
        let theme = theme();
        let context = egui::Context::default();
        // The first pass lays the button out; its rectangle is fixed after
        // that, so the pointer can aim at the centre.
        let mut rect = Rect::ZERO;
        pass(&context, Vec::new(), 0, |ui| {
            let response = ui.add(ChunkyButton::new("Press", &theme).accent(true));
            rect = response.rect;
            response
        });
        assert!(rect.is_positive(), "the button was laid out");
        let mut clicked = false;
        for (index, events) in press_at(rect.center()).into_iter().enumerate() {
            pass(&context, events, index + 1, |ui| {
                let response = ui.add(ChunkyButton::new("Press", &theme).accent(true));
                clicked |= response.clicked();
                response
            });
        }
        assert!(clicked, "the press and release did not make a click");
    }

    #[test]
    fn a_disabled_button_reports_no_click() {
        let theme = theme();
        let context = egui::Context::default();
        let mut rect = Rect::ZERO;
        let output = pass(&context, Vec::new(), 0, |ui| {
            let response = ui.add_enabled(false, ChunkyButton::new("Off", &theme));
            rect = response.rect;
            assert!(!response.enabled(), "the response knows it is disabled");
            response
        });
        assert!(rect.is_positive());
        let mut clicked = false;
        for (index, events) in press_at(rect.center()).into_iter().enumerate() {
            pass(&context, events, index + 1, |ui| {
                let response = ui.add_enabled(false, ChunkyButton::new("Off", &theme));
                clicked |= response.clicked();
                response
            });
        }
        assert!(!clicked, "a disabled button reported a click");
        // A disabled `Ui` fades everything painted in it, so the face reads as
        // the panel fill at half alpha rather than as any live state's colour.
        let painted = faces(&output);
        let face = widest(&painted).expect("the button painted a face");
        assert!(face.a() < 255, "a disabled face is faded: {face:?}");
        assert!(
            face.r() <= color(theme.palette.panel).r(),
            "a disabled face is no brighter than the panel: {face:?}"
        );
        assert_ne!(face, color(theme.palette.panel_raised));
        assert_ne!(face, color(theme.palette.accent));
    }

    /// The face's fill is the token the state names: raised at rest, control
    /// while the pointer is over it, line while held.
    #[test]
    fn hover_and_press_use_the_state_tokens() {
        let theme = theme();
        let context = egui::Context::default();
        let mut rect = Rect::ZERO;
        let rest = pass(&context, Vec::new(), 0, |ui| {
            let response = ui.add(ChunkyButton::new("Idle", &theme));
            rect = response.rect;
            response
        });
        assert_eq!(
            widest(&faces(&rest)),
            Some(color(theme.palette.panel_raised)),
            "the resting face is the raised control fill"
        );

        let center = rect.center();
        let hovered = pass(&context, vec![Event::PointerMoved(center)], 1, |ui| {
            ui.add(ChunkyButton::new("Idle", &theme))
        });
        assert_eq!(
            widest(&faces(&hovered)),
            Some(color(theme.palette.control)),
            "hovering uses the control fill"
        );

        let held = pass(
            &context,
            vec![Event::PointerButton {
                pos: center,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            2,
            |ui| ui.add(ChunkyButton::new("Idle", &theme)),
        );
        assert_eq!(
            widest(&faces(&held)),
            Some(color(theme.palette.line)),
            "holding the button uses the pressed fill"
        );
    }

    #[test]
    fn a_clipped_button_paints_nothing() {
        let theme = theme();
        // A one-pixel screen: the button is allocated below the visible area.
        let context = egui::Context::default();
        let mut output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1.0, 1.0))),
                ..Default::default()
            },
            |ui| {
                ui.add_space(200.0);
                ui.add(ChunkyButton::new("Off screen", &theme).accent(true));
            },
        );
        output.textures_delta.clear();
        assert!(
            output.shapes.is_empty(),
            "a clipped button drew something: {:?}",
            output.shapes.len()
        );
    }
}

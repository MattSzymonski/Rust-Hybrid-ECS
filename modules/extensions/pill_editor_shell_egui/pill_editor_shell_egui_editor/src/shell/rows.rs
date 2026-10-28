//! The editor's one row layout: a label column and the row's control.
//!
//! # Responsibilities
//!
//! - [`ROW_LABEL_WIDTH`] and [`WIDE_ROW_LABEL_WIDTH`]: the two label columns,
//!   the same in every tab that uses them, so the controls down a panel line
//!   up at one x.
//! - [`row`] and [`row_in`]: one label/control row, the default way a field
//!   draws.
//! - [`label_cell`] and [`label_cell_in`]: the label alone, for a row whose
//!   control is a wrapped or nested run the tab builds by hand.
//! - [`value_row`] and [`value_row_in`]: a read-only row whose control is the
//!   value as monospace text.
//!
//! # Design
//!
//! A row is a layout, never a fill: nothing paints a background behind it, so
//! the panel's own surface shows through and rows never stripe alternately.
//! The label cell is fixed and its text starts at the cell's left edge, so the
//! labels of a panel read as one column and a long one is truncated inside its
//! column instead of shifting its neighbours. A row keeps
//! [`ROW_LABEL_GAP`] points between the label and its control whatever the
//! panel's item spacing is, and every tab keeps the stock widget heights it
//! places. A panel whose labels are field names takes the compact column
//! ([`ROW_LABEL_WIDTH`]); a panel whose labels are prose takes the wide one
//! ([`WIDE_ROW_LABEL_WIDTH`]).
//!
//! The globs below are absolute paths: a generated patch crate replays this
//! file's top-level `use` lines, so a `super::`-relative glob would not
//! resolve there.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The width of a row's label column where labels are field names, in points:
/// the Inspector's dense rows, and a module panel's control points.
pub const ROW_LABEL_WIDTH: f32 = 80.0;

/// The width of a row's label column where labels are prose, in points: the
/// Preferences, Systems and Statistics rows name a whole setting instead of a
/// field, so their column fits the longest of them while leaving the narrow
/// Preferences panel the rest of its width for the control.
pub const WIDE_ROW_LABEL_WIDTH: f32 = 140.0;

/// The height a row's label cell takes, in points: about a stock control's,
/// so a checkbox, a drag value and a button all read as one row.
pub const ROW_HEIGHT: f32 = 18.0;

/// The least space a row keeps between its label and its control, in points:
/// the panel's own item spacing is far too tight for a label to read as naming
/// the control beside it.
pub const ROW_LABEL_GAP: f32 = 20.0;

/// One row's label in the compact column, left-aligned, truncated to
/// [`ROW_LABEL_WIDTH`], with `tooltip` once the pointer rests on it.
pub fn label_cell(ui: &mut egui::Ui, text: &str, tooltip: Option<&str>) -> egui::Response {
    label_cell_in(ui, ROW_LABEL_WIDTH, text, tooltip)
}

/// One row's label in a column `width` points wide.
///
/// The cell is allocated as its own rectangle and the label laid out from its
/// left edge: a label placed by the row itself is centred in whatever width the
/// cell has, so a short name would drift away from the column it belongs to.
pub fn label_cell_in(
    ui: &mut egui::Ui,
    width: f32,
    text: &str,
    tooltip: Option<&str>,
) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::hover());
    let mut label = ui
        .new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        )
        .add(egui::Label::new(egui::RichText::new(text).color(style::text_muted())).truncate());
    if let Some(tooltip) = tooltip {
        label = label.on_hover_text(tooltip);
    }
    label
}

/// The space a row adds after its label cell, which with the run's own item
/// spacing reaches [`ROW_LABEL_GAP`].
pub fn label_gap(ui: &mut egui::Ui) {
    let spacing = ui.spacing().item_spacing.x;
    ui.add_space((ROW_LABEL_GAP - spacing).max(0.0));
}

/// One label/control row in the compact column: `text` in the label column,
/// then the controls `control` draws beside it.
pub fn row(
    ui: &mut egui::Ui,
    text: &str,
    tooltip: Option<&str>,
    control: &mut dyn FnMut(&mut egui::Ui),
) {
    row_in(ui, ROW_LABEL_WIDTH, text, tooltip, control);
}

/// One label/control row whose label column is `width` points wide.
pub fn row_in(
    ui: &mut egui::Ui,
    width: f32,
    text: &str,
    tooltip: Option<&str>,
    control: &mut dyn FnMut(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        label_cell_in(ui, width, text, tooltip);
        label_gap(ui);
        control(ui);
    });
}

/// One read-only row: the label, then `value` as monospace text.
pub fn value_row(ui: &mut egui::Ui, text: &str, value: &str) {
    value_row_in(ui, ROW_LABEL_WIDTH, text, value);
}

/// One read-only row whose label column is `width` points wide.
pub fn value_row_in(ui: &mut egui::Ui, width: f32, text: &str, value: &str) {
    row_in(ui, width, text, None, &mut |ui| {
        ui.monospace(value);
    });
}

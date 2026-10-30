//! The windows module panels ask the shell for.
//!
//! # Responsibilities
//!
//! - [`panel_windows`]: draw every window a panel opened, by calling that
//!   panel's own draw function with the window's id, so a panel's dialog body
//!   and its tab body are the same code.
//! - Answer a modal with Enter or Escape and leave the answer for the panel.
//! - Close the windows of a module whose panels are gone, every frame, so a
//!   window from an unloaded image is never drawn.
//! - Publish each window's rectangle into the frame's overlay regions: a click
//!   on a window is the UI's, never a scene pick.
//!
//! # Design
//!
//! The shell owns the window (position, lifetime, modality) and the panel owns
//! the content. Windows live in [`PanelWindowSession`], a session resource, so
//! a shell reload keeps them open. A window closes when the user closes it,
//! when a modal is answered, or when the owning module unloads and its draw
//! function's image is gone; a panel that wants it back asks again on its next
//! draw.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
// Deliberately unused in this compilation: a patch crate replays this
// file's opening, and the first sibling this body names resolves through it.
#[allow(unused_imports)]
use pill_editor_shell_egui_editor::shell::*;

/// The panel data every window draw reads, gathered once per pass.
pub struct PanelFeed {
    /// The world's entities, as the panels see them.
    pub entities: Vec<PanelEntityRow>,
    /// The selected entity's components, when one is selected.
    pub detail: Option<PanelEntityDetail>,
    /// The editor's smoothed frame rate.
    pub frames_per_second: f32,
}

/// Draw every open panel window and publish its rectangle.
///
/// `commands` collects what the windows ask for; the caller forwards them to
/// the executable at the frame boundary. A window command queued from inside
/// a window has no slot to attribute it to, so only a tab body's own requests
/// open or close windows.
pub fn panel_windows(
    context: &egui::Context,
    windows: &mut PanelWindowSession,
    registry: &PanelRegistry,
    budget: &mut PanelBudget,
    feed: &PanelFeed,
    commands: &mut Vec<PanelCommand>,
    regions: &mut Vec<ShellRect>,
) {
    // A window from a module whose panels are gone has nothing to draw with:
    // the draw function's image is unmapped, so the window goes with it.
    windows.windows.retain(|window| registry.alive(window.slot));
    if windows.windows.is_empty() {
        return;
    }
    let snapshot = windows.windows.clone();
    for window in &snapshot {
        let Some(draw) = registry.draw_of(window.slot) else {
            continue;
        };
        let answer = windows
            .answer
            .as_ref()
            .map(|answer| pill_editor_core::panels::PanelAnswer {
                window: answer.window.clone(),
                accepted: answer.accepted,
            });
        let mut body = |ui: &mut egui::Ui, commands: &mut Vec<PanelCommand>| {
            let mut panel_context = PanelContext {
                frames_per_second: feed.frames_per_second,
                selected_entity: feed.detail.as_ref().map(|detail| detail.entity),
                entities: feed.entities.as_slice(),
                detail: feed.detail.as_ref(),
                window: Some(window.id.as_str()),
                requested_window: None,
                answer: answer.as_ref(),
                commands,
            };
            let started = pill_core::platform::Instant::now();
            draw(ui, &mut panel_context);
            let micros = started.elapsed().as_micros() as f32;
            budget.record(window.slot, micros);
        };
        if window.modal {
            // A modal covers the editor and takes its input: egui's own modal
            // layer dims what is behind it and swallows the clicks, so the
            // dock cannot be used while it is open.
            let modal = egui::Modal::new(egui::Id::new(("pill_editor.panel_modal", &window.id)))
                .show(context, |ui| {
                    ui.set_width(window.width);
                    ui.heading(window.title.as_str());
                    ui.separator();
                    body(ui, commands);
                    ui.separator();
                    // The modal's two answers wear the kit's chunky face, the
                    // accept one in the accent: a modal is the one place the
                    // editor asks a question, so its answers are the loudest
                    // buttons on screen.
                    let theme = style::current_theme();
                    let accepted = ui
                        .horizontal(|ui| {
                            let accept = ui
                                .add(kit::ChunkyButton::new("Apply", &theme).accent(true))
                                .clicked();
                            let cancel = ui.add(kit::ChunkyButton::new("Cancel", &theme)).clicked();
                            (accept, cancel)
                        })
                        .inner;
                    let keys = ui.input(|input| {
                        (
                            input.key_pressed(egui::Key::Enter),
                            input.key_pressed(egui::Key::Escape),
                        )
                    });
                    if accepted.0 || keys.0 {
                        return Some(true);
                    }
                    if accepted.1 || keys.1 {
                        return Some(false);
                    }
                    None
                })
                .inner;
            if let Some(accepted) = modal {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    window = window.id.as_str(),
                    accepted,
                    "[editor] panel modal answered"
                );
                windows.answer = Some(PanelWindowAnswer {
                    window: window.id.clone(),
                    source: window.source.clone(),
                    accepted,
                });
                windows.close(&window.source, &window.id);
            }
            // A modal blocks the editor's input for as long as it is open, so
            // what lies behind it cannot be clicked while it is up; the
            // region list already carries every chip the dock drew, and a
            // modal covers them.
            continue;
        }
        let mut open = true;
        let response = egui::Window::new(window.title.as_str())
            .id(egui::Id::new(("pill_editor.panel_window", &window.id)))
            .open(&mut open)
            .default_width(window.width)
            .default_height(window.height)
            .resizable(true)
            .show(context, |ui| {
                ui.set_width(window.width);
                body(ui, commands);
            });
        if let Some(response) = response {
            let rect = response.response.rect;
            regions.push(ShellRect {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.width(),
                height: rect.height(),
            });
        }
        if !open {
            windows.close(&window.source, &window.id);
        }
    }
}

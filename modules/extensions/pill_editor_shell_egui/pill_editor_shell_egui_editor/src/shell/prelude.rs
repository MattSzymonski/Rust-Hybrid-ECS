//! The names every shell module shares, re-exported once.
//!
//! # Responsibilities
//!
//! - Re-export the engine, UI and editor-core types the shell's modules name,
//!   so each of them opens with two globs instead of a long import list.
//!
//! # Design
//!
//! A split module writes `use pill_editor_shell_egui_editor::shell::prelude::*;`
//! and `use pill_editor_shell_egui_editor::shell::*;`, both by absolute path:
//! a generated patch crate replays the edited file's top-level `use` lines,
//! and a `super::`-relative glob would not resolve inside that crate.

// External crates
pub use pill_editor_core_egui::assets::{self, AssetDialog};
pub use pill_editor_core_egui::browser::{self, ProjectBrowser};
pub use pill_editor_core_egui::commands::{CommandView, PendingEditorCommands};
pub use pill_editor_core_egui::console::{
    self as console, ConsoleChannel, ConsoleFilterSettings, ConsoleLine, ConsoleView,
};
pub use pill_editor_core_egui::editor_state::{
    AssetListing, ComponentView, EditorCommand, EditorSelection, EditorSnapshot, EntityDetail,
    ResourceFieldView, ResourceView,
};
pub use pill_editor_core_egui::entities;
pub use pill_editor_core_egui::gizmo::{GizmoMode, GizmoSpace};
pub use pill_editor_core_egui::inspector;
pub use pill_editor_core_egui::layout::EditorTab;
pub use pill_editor_core_egui::menubar;
pub use pill_editor_core_egui::notices::{EditorNotices, Notice, NoticeLevel, ProblemRow};
pub use pill_editor_core_egui::preferences;
pub use pill_editor_core_egui::session::{
    EguiShellState, InspectorSession, MenubarSession, PaletteState, PanelBudget, PanelTiming,
    PanelWindowAnswer, PanelWindowSession, PanelWindowState, StatisticsSession, Toast,
    ToastSession, ToolboxSession, PANEL_BUDGET_FRAMES, PANEL_BUDGET_MICROS, PANEL_WINDOWS_MAX,
    TOASTS_MAX, TOAST_FRAMES,
};
pub use pill_editor_core_egui::systems;
pub use pill_editor_core_egui::toolbox;
pub use pill_engine::{pill_hot_fn, Entity, FieldValue, Input};
pub use pill_ui_egui::boundary::{
    frame_from_output, EguiShellFrame, EguiShellInput, ShellRect, SCENE_VIEWPORTS,
};
pub use pill_ui_egui::egui;
pub use pill_ui_egui::egui_dock::{
    DockArea, DockState, NodeIndex, NodePath, SurfaceIndex, TabViewer,
};
pub use pill_ui_egui::icons;
pub use pill_ui_egui::panels::{
    PanelCommand, PanelComponentValues, PanelContext, PanelDrawFn, PanelEntityDetail,
    PanelEntityRow, PanelFieldValue, PanelRegistry, PanelWindowRequest,
};
pub use pill_ui_egui::style;
pub use serde_json::Value as JsonValue;

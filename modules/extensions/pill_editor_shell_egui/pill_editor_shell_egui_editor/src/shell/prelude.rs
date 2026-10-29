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
pub use pill_editor_core::assets::{self, AssetDialog};
pub use pill_editor_core::boundary::{
    frame_from_output, EguiShellFrame, EguiShellInput, ShellBadge, ShellRect, BADGES_PER_VIEW_MAX,
    DEFAULT_SCENE_VIEWS, SCENE_VIEWS_MAX,
};
pub use pill_editor_core::browser::{self, ProjectBrowser};
pub use pill_editor_core::commands::{CommandView, PendingEditorCommands};
pub use pill_editor_core::console::{
    self as console, ConsoleChannel, ConsoleFilterSettings, ConsoleLine, ConsoleView,
};
pub use pill_editor_core::editor_state::{
    AssetKind, AssetListing, AssetReferenceField, AssetReferenceLabel, ComponentView,
    EditorCommand, EditorSelection, EditorSnapshot, EngineAsset, EntityDetail, EntityListEntry,
    EntityReferenceLabel, ResourceFieldView, ResourceView,
};
pub use pill_editor_core::egui;
pub use pill_editor_core::egui_dock::{
    DockArea, DockState, NodeIndex, NodePath, SurfaceIndex, TabViewer,
};
pub use pill_editor_core::entities;
pub use pill_editor_core::gizmo::{GizmoMode, GizmoSpace};
pub use pill_editor_core::icons;
pub use pill_editor_core::inspector;
pub use pill_editor_core::layout::EditorTab;
pub use pill_editor_core::menubar;
pub use pill_editor_core::notices::{EditorNotices, Notice, NoticeLevel, ProblemRow};
pub use pill_editor_core::panels::{
    PanelCommand, PanelComponentValues, PanelContext, PanelDrawFn, PanelEntityDetail,
    PanelEntityRow, PanelFieldValue, PanelRegistry, PanelWindowRequest,
};
pub use pill_editor_core::preferences;
pub use pill_editor_core::session::{
    EguiShellState, InspectorSession, MenubarSession, PaletteState, PanelBudget, PanelTiming,
    PanelWindowAnswer, PanelWindowSession, PanelWindowState, StatisticsSession, Toast,
    ToastSession, ToolboxSession, PANEL_BUDGET_FRAMES, PANEL_BUDGET_MICROS, PANEL_WINDOWS_MAX,
    TOASTS_MAX, TOAST_FRAMES,
};
pub use pill_editor_core::style;
pub use pill_editor_core::systems;
pub use pill_editor_core::toolbox;
pub use pill_engine::{pill_hot_fn, Entity, FieldValue, Input};
pub use serde_json::Value as JsonValue;

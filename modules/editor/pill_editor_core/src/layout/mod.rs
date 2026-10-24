//! The egui editor's dock layout: an `egui_dock` state over [`EditorTab`]s.
//!
//! # Responsibilities
//!
//! - Present the layout vocabulary the shell and persistence share: the tab
//!   set ([`dock`]) and the on-disk document ([`persistence`]).
//! - Re-export the surface here.
//!
//! # Design
//!
//! Replaces the copied hand-made dock model (W-0017 Stage 5): `egui_dock`
//! owns the splits, tab geometry and drag behavior, so this module only
//! defines the editor's tabs, the default layout and where the document
//! lives. The dock value itself lives in the shell's session resource; this
//! crate only defines types and persistence, so it stays UI-thin.

mod dock;
mod persistence;
mod recovery;

pub use dock::{activate_tab, EditorDock, EditorTab, BOTTOM_ROW_FRACTION, SIDE_COLUMN_FRACTION};
pub use persistence::{
    layout_directory, load_or_default, save, LayoutError, LAYOUT_SCHEMA_VERSION,
};
pub use recovery::{
    read as read_recovery, recovery_path, remove as remove_recovery, write as write_recovery,
    RecoveryDocument, RECOVERY_FILE, RECOVERY_VERSION,
};

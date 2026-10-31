//! The resources the editor lets a person inspect and edit.
//!
//! # Responsibilities
//!
//! - List the world resources that are *editable*: the engine keeps no
//!   resource reflection, so a resource joins this list by having a curated
//!   field table beside it (the gizmo settings, in
//!   [`crate::gizmo::set_resource_field`]).
//! - Read those fields into plain data the Inspector can draw, and name the
//!   label each row shows.
//!
//! # Design
//!
//! The table is the single source of the row order, the labels and the field
//! names a write goes through, so the tab cannot offer a field the write path
//! would refuse. Adding a resource is one entry here plus its own read/write
//! table, the shape the gizmo's has.

// Current crate
use crate::gizmo;

/// One editable world resource.
#[derive(Clone, Copy, Debug)]
pub struct ResourceEntry {
    /// The stable name a command addresses it by - its type path.
    pub name: &'static str,
    /// The label the Inspector's section shows.
    pub label: &'static str,
    /// The editable fields, in row order: the resource's own table, the same
    /// one its write path goes through.
    pub fields: &'static [gizmo::ResourceField],
}

/// The resources the Inspector can show, in list order.
pub const EDITABLE_RESOURCES: &[ResourceEntry] = &[ResourceEntry {
    name: gizmo::RESOURCE_NAME,
    label: "Gizmo Settings",
    fields: gizmo::RESOURCE_FIELDS,
}];

/// The resource an entry names, when the list has one.
pub fn entry(name: &str) -> Option<&'static ResourceEntry> {
    EDITABLE_RESOURCES.iter().find(|entry| entry.name == name)
}

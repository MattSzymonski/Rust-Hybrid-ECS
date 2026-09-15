//! Periodic ECS state report, registered by the engine itself.
//!
//! # Responsibilities
//!
//! - Gather a snapshot of world state: entities, archetypes, columns, the
//!   component registry, systems and resources.
//! - Render it as a fixed-width block and print it every N frames.
//! - Register that as an engine-owned system so it survives every hot reload.
//!
//! # Design
//!
//! This runs as a real scheduler system rather than a call inside
//! `process_frame`, so it appears in the system list, is attributed like any
//! other system, and can be disabled. It needs to read the whole world -
//! archetypes, registry, resources - which no [`SystemParam`](crate::SystemParam)
//! exposes, so it is registered through
//! [`register_system_with_access`](crate::Engine::register_system_with_access)
//! with an access declaration that conflicts with everything. That is what
//! makes reading the entire world sound: a system that conflicts with every
//! other is never placed in a parallel batch.
//!
//! It is owned by [`SystemOwner::ENGINE`](crate::SystemOwner::ENGINE), which
//! no reload path clears, so the report keeps running across project and
//! module reloads.
//!
//! ## What it cannot see
//!
//! The module **graveyard** - retired DLL images kept mapped while their rows
//! migrate - belongs to `pill_host`, which `pill_engine` does not depend on.
//! It is not reachable from here. The engine's own equivalents are reported
//! instead: recycled entity IDs, and archetypes that are still allocated but
//! hold no entities.

// Standard library
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

// Current crate
use crate::component::ComponentId;
use crate::world::World;

/// How often the report prints when the engine registers it.
///
/// Wall-clock rather than a frame count: a frame count prints at whatever rate
/// the project happens to run at, which is every 60 ms on a 1600 fps demo and
/// every two seconds on a 50 fps one. A duration reads the same everywhere.
pub const DEFAULT_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Name the diagnostics system is registered under.
pub const SYSTEM_NAME: &str = "engine_ecs_diagnostics";

// =============================================================================
// Snapshot
// =============================================================================

/// One stored component column within an archetype.
struct ColumnReport {
    /// Registry bit this component occupies, which is the bit it contributes
    /// to the owning archetype's mask.
    bit: Option<u8>,
    /// Registered name of the component.
    name: String,
    /// Bytes one row of this column occupies.
    stride: usize,
    /// Whether the column is byte-oriented storage owned by another language.
    dynamic: bool,
}

/// One archetype's contribution to the report.
struct ArchetypeReport {
    /// The archetype's packed component mask, which is its identity.
    id: u128,
    /// Entities currently stored in it.
    entities: usize,
    /// Its columns, ordered by mask bit so they read in the same order the
    /// mask's set bits do.
    columns: Vec<ColumnReport>,
    /// Bytes every column occupies together.
    total_bytes: usize,
}

/// Everything the report shows, gathered in one pass over the world.
///
/// Collected into an owned structure rather than formatted inline so the
/// world borrow ends before any printing happens, and so the gathering can be
/// tested without capturing stdout.
pub struct EcsSnapshot {
    /// Frame this snapshot was taken on.
    pub frame: u64,
    /// Seconds since startup.
    pub elapsed_seconds: f32,
    /// Milliseconds the previous frame took.
    pub last_frame_milliseconds: f32,
    /// Live entities across every archetype.
    pub entities: usize,
    /// Entity IDs retired and available for reuse.
    pub recycled_entity_ids: usize,
    /// Archetypes the world holds, including empty ones.
    pub archetypes: usize,
    /// Archetypes still allocated but holding no entities.
    pub empty_archetypes: usize,
    /// Component types registered, of the 128 a `ComponentMask` can hold.
    pub registered_components: usize,
    /// Registrations still available before the 128-type ceiling.
    pub available_component_slots: usize,
    /// Registered components that declare a cross-binary shared identity.
    pub shared_components: Vec<String>,
    /// Resources whose type declares a cross-binary shared identity, by
    /// declared name.
    ///
    /// A shared resource has no `TypeId` naming it in every artifact, so the
    /// name it declared is the only thing there is to report.
    pub shared_resources: Vec<String>,
    /// Registered components defined by another language at runtime.
    pub dynamic_components: Vec<String>,
    /// Registered component types no archetype currently stores.
    pub unused_components: Vec<String>,
    /// The world's current change tick.
    pub change_tick: u32,
    /// Resources held by the world.
    pub resources: usize,
    /// Per-archetype detail, largest first.
    archetype_reports: Vec<ArchetypeReport>,
}

impl EcsSnapshot {
    /// Gather the report from a world.
    ///
    /// Read-only: nothing here touches a change tick, so the report cannot
    /// itself make a `Changed<T>` filter fire.
    pub fn gather(world: &World) -> Self {
        let registry = world.component_registry();

        // Step 1: Name every registered component once, and note which ones
        // carry an identity other than a plain per-binary `TypeId`.
        let mut names: BTreeMap<ComponentId, String> = BTreeMap::new();
        let mut shared_components = Vec::new();
        let mut dynamic_components = Vec::new();
        for (component_id, _bit, name) in registry.registered_components() {
            names.insert(component_id, name.to_string());
            match component_id {
                ComponentId::Shared(_) => shared_components.push(name.to_string()),
                ComponentId::Dynamic(_) => dynamic_components.push(name.to_string()),
                ComponentId::Native(_) => {}
            }
        }
        shared_components.sort();
        dynamic_components.sort();

        // Step 2: Walk the archetypes, accumulating totals and per-archetype
        // detail, and recording which components are actually stored anywhere.
        let mut used: Vec<ComponentId> = Vec::new();
        let mut archetype_reports = Vec::new();
        let mut entities = 0;
        let mut empty_archetypes = 0;

        for archetype in world.archetypes_iter() {
            let entity_count = archetype.entities.len();
            entities += entity_count;
            if entity_count == 0 {
                empty_archetypes += 1;
            }

            let mut columns = Vec::with_capacity(archetype.component_types.len());
            let mut total_bytes = 0;
            for &component_id in &archetype.component_types {
                if !used.contains(&component_id) {
                    used.push(component_id);
                }
                // Row bytes, not capacity: what the stored rows actually
                // occupy, so the total tracks entities rather than allocation.
                let (stride, rows, dynamic) =
                    if let Some(column) = archetype.component_storages.get(component_id) {
                        (column.elem_size(), column.len(), false)
                    } else if let Some(column) =
                        archetype.dynamic_component_storages.get(&component_id)
                    {
                        (column.element_size(), column.len(), true)
                    } else {
                        (0, 0, false)
                    };
                total_bytes += stride * rows;
                columns.push(ColumnReport {
                    bit: registry.get_bit(&component_id),
                    name: names
                        .get(&component_id)
                        .cloned()
                        .unwrap_or_else(|| format!("{component_id:?}")),
                    stride,
                    dynamic,
                });
            }
            // By mask bit, so the listing reads in the same order the set bits
            // of the archetype's id do.
            columns.sort_by_key(|column| (column.bit, column.name.clone()));

            archetype_reports.push(ArchetypeReport {
                id: archetype.id.0,
                entities: entity_count,
                columns,
                total_bytes,
            });
        }

        // Largest first: a report that is truncated should keep the archetypes
        // carrying the most rows.
        archetype_reports.sort_by_key(|report| std::cmp::Reverse(report.entities));

        // Step 3: A registered component no archetype stores is worth showing -
        // it is either newly registered or orphaned by a reload.
        let mut unused_components: Vec<String> = names
            .iter()
            .filter(|(component_id, _)| !used.contains(component_id))
            .map(|(_, name)| name.clone())
            .collect();
        unused_components.sort();

        let (frame, elapsed_seconds, last_frame_milliseconds) =
            match world.get_resource::<crate::time::Time>() {
                Some(time) => (
                    time.frame_count(),
                    time.elapsed_seconds(),
                    time.last_frame_milliseconds(),
                ),
                None => (0, 0.0, 0.0),
            };

        // Step 4: Resources carry no registry entry, so their shared identities
        // come from the names their types declared.
        let shared_resources = world.shared_resource_names();

        Self {
            frame,
            elapsed_seconds,
            last_frame_milliseconds,
            entities,
            recycled_entity_ids: world.recycled_entity_id_count(),
            archetypes: archetype_reports.len(),
            empty_archetypes,
            registered_components: registry.len(),
            available_component_slots: registry.available_slots(),
            shared_components,
            shared_resources,
            dynamic_components,
            unused_components,
            change_tick: world.change_tick().get(),
            resources: world.resource_count(),
            archetype_reports,
        }
    }

    /// Render the snapshot as the block the engine prints.
    ///
    /// Returned as a `String` rather than printed directly so a caller can
    /// route it somewhere other than stdout, and so it can be asserted on.
    pub fn render(&self) -> String {
        /// Inner width of the block, in characters.
        const WIDTH: usize = 74;

        // One helper pads every content line to the same width, so the borders
        // line up whatever the content is. Hand-tuned per-field widths drift
        // the moment a number grows a digit.
        fn row(out: &mut String, content: &str) {
            let length = content.chars().count();
            if length >= WIDTH {
                let clipped: String = content.chars().take(WIDTH - 1).collect();
                let _ = writeln!(out, "│{clipped}…│");
            } else {
                let _ = writeln!(out, "│{content}{}│", " ".repeat(WIDTH - length));
            }
        }

        // Two-part line: `left` runs from the margin, `right` is flushed to
        // the far edge, and the gap between them absorbs the slack. The left
        // side is what gets truncated, because the right side is the number
        // and losing its digits would be worse than losing a name's prefix.
        fn split_row(out: &mut String, left: &str, right: &str) {
            // One space before the border, mirroring the margin the left side
            // starts from, so the figure is not pressed against the edge.
            const RIGHT_MARGIN: usize = 1;
            let right_length = right.chars().count() + RIGHT_MARGIN;
            let available = WIDTH.saturating_sub(right_length + 2);
            let left_length = left.chars().count();
            let margin = " ".repeat(RIGHT_MARGIN);
            if left_length > available {
                let clipped: String = left.chars().take(available.saturating_sub(1)).collect();
                let _ = writeln!(out, "│{clipped}…  {right}{margin}│");
            } else {
                let padding = WIDTH - left_length - right_length;
                let _ = writeln!(out, "│{left}{}{right}{margin}│", " ".repeat(padding));
            }
        }

        let mut out = String::with_capacity(1024);
        let rule = "─".repeat(WIDTH);

        let _ = writeln!(out, "┌{rule}┐");
        row(
            &mut out,
            &format!(
                " ECS state · frame {} · {:.1}s elapsed · last frame {:.2} ms",
                self.frame, self.elapsed_seconds, self.last_frame_milliseconds
            ),
        );
        let _ = writeln!(out, "├{rule}┤");

        // Totals: the numbers worth seeing even when nothing else is read.
        row(
            &mut out,
            &format!(
                "  entities {:<8} archetypes {:<7} components {:<6} resources {}",
                self.entities, self.archetypes, self.registered_components, self.resources
            ),
        );
        row(
            &mut out,
            &format!(
                "  recycled ids {:<4} empty arch {:<7} free slots {:<6} tick {}",
                self.recycled_entity_ids,
                self.empty_archetypes,
                self.available_component_slots,
                self.change_tick
            ),
        );

        // Archetypes, largest first. Each one is headed by a sentence rather
        // than a column of bare numbers: the previous layout printed
        // `21 entities  756 B  archetype 0x2a` over an unlabelled list, and
        // nothing on the line said the hex was a component mask, what the
        // bytes counted, or that the indented names were its columns.
        let _ = writeln!(out, "├{rule}┤");
        if self.archetype_reports.is_empty() {
            row(&mut out, "  no archetypes");
        } else {
            row(
                &mut out,
                "  an archetype's id is its component mask; each column shows its bit",
            );
            for archetype in &self.archetype_reports {
                let _ = writeln!(out, "├{rule}┤");
                row(
                    &mut out,
                    &format!(
                        "  archetype 0x{:x} · {} {} · {} in {} column{}",
                        archetype.id,
                        archetype.entities,
                        if archetype.entities == 1 { "entity" } else { "entities" },
                        format_bytes(archetype.total_bytes),
                        archetype.columns.len(),
                        if archetype.columns.len() == 1 { "" } else { "s" },
                    ),
                );
                for column in &archetype.columns {
                    // `rows x stride` rather than a product, so a column whose
                    // element is unexpectedly large is visible as such.
                    let size = format!(
                        "{} × {}{}",
                        archetype.entities,
                        format_bytes(column.stride),
                        if column.dynamic { " (dyn)" } else { "" },
                    );
                    let bit = match column.bit {
                        Some(bit) => format!("bit {bit:>3}"),
                        None => "bit   ?".to_string(),
                    };
                    split_row(&mut out, &format!("    {bit}  {}", column.name), &size);
                }
            }
        }

        // Only show the identity breakdowns that have anything in them; an
        // empty list is noise, not information.
        let mut sections: Vec<(&str, &Vec<String>)> = Vec::new();
        if !self.shared_components.is_empty() {
            sections.push(("shared identity", &self.shared_components));
        }
        if !self.shared_resources.is_empty() {
            sections.push(("shared resources", &self.shared_resources));
        }
        if !self.dynamic_components.is_empty() {
            sections.push(("runtime-defined", &self.dynamic_components));
        }
        if !self.unused_components.is_empty() {
            sections.push(("registered, unused", &self.unused_components));
        }
        for (label, entries) in sections {
            let _ = writeln!(out, "├{rule}┤");
            row(&mut out, &format!("  {label}"));
            for name in entries {
                row(&mut out, &format!("    · {name}"));
            }
        }

        let _ = write!(out, "└{rule}┘");
        out
    }
}

/// Format a byte count with a unit, so a column total reads at a glance.
fn format_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;
    use crate::World;

    #[derive(Clone, Debug)]
    struct Position {
        _x: f32,
        _y: f32,
    }
    impl Component for Position {}
    trait_type_map::impl_trait_accessible!(dyn Component; Position);

    #[derive(Clone, Debug)]
    struct Velocity {
        _x: f32,
    }
    impl Component for Velocity {}
    trait_type_map::impl_trait_accessible!(dyn Component; Velocity);

    /// An empty world reports zeroes rather than failing, and says so in place
    /// of printing an empty archetype list.
    #[test]
    fn an_empty_world_renders() {
        let world = World::new();
        let snapshot = EcsSnapshot::gather(&world);

        assert_eq!(snapshot.entities, 0);
        assert_eq!(snapshot.archetypes, 0);
        assert_eq!(snapshot.registered_components, 0);
        assert!(snapshot.render().contains("no archetypes"));
    }

    /// Totals and per-archetype detail reflect what the world holds, and every
    /// component name reaches the rendered block.
    #[test]
    fn the_snapshot_counts_what_the_world_holds() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        for index in 0..5 {
            world
                .create_entity()
                .with(Position {
                    _x: index as f32,
                    _y: 0.0,
                })
                .build()
                .unwrap();
        }
        // A second archetype: these carry both components.
        for _ in 0..3 {
            world
                .create_entity()
                .with(Position { _x: 0.0, _y: 0.0 })
                .with(Velocity { _x: 1.0 })
                .build()
                .unwrap();
        }

        let snapshot = EcsSnapshot::gather(&world);
        assert_eq!(snapshot.entities, 8);
        assert_eq!(snapshot.archetypes, 2);
        assert_eq!(snapshot.registered_components, 2);
        assert_eq!(snapshot.empty_archetypes, 0);
        assert!(snapshot.unused_components.is_empty());

        let rendered = snapshot.render();
        assert!(rendered.contains("Position"), "{rendered}");
        assert!(rendered.contains("Velocity"), "{rendered}");
        // Largest archetype first.
        let position_only = rendered.find("5 entities").expect("5-entity archetype");
        let both = rendered.find("3 entities").expect("3-entity archetype");
        assert!(position_only < both, "archetypes must be ordered by size");
    }

    /// A component that is registered but stored nowhere is called out, which
    /// is what an orphaned type looks like after a reload.
    #[test]
    fn a_registered_but_unstored_component_is_reported() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world
            .create_entity()
            .with(Position { _x: 0.0, _y: 0.0 })
            .build()
            .unwrap();

        let snapshot = EcsSnapshot::gather(&world);
        assert_eq!(snapshot.unused_components.len(), 1);
        assert!(snapshot.unused_components[0].ends_with("Velocity"));
        assert!(snapshot.render().contains("registered, unused"));
    }

    /// Every rendered line is the same width, so the block's borders line up
    /// in a terminal regardless of what the world contains.
    #[test]
    fn every_rendered_line_is_the_same_width() {
        let mut world = World::new();
        world.register_component::<Position>();
        world
            .create_entity()
            .with(Position { _x: 0.0, _y: 0.0 })
            .build()
            .unwrap();

        let rendered = EcsSnapshot::gather(&world).render();
        let widths: Vec<usize> = rendered.lines().map(|line| line.chars().count()).collect();
        assert!(!widths.is_empty());
        assert!(
            widths.iter().all(|width| *width == widths[0]),
            "line widths differ: {widths:?}\n{rendered}"
        );
    }

    /// Gathering the report must not disturb change detection: it is called
    /// every frame, and a report that bumped a tick would make every
    /// `Changed<T>` filter fire.
    #[test]
    fn gathering_does_not_touch_the_change_tick() {
        let mut world = World::new();
        world.register_component::<Position>();
        world
            .create_entity()
            .with(Position { _x: 0.0, _y: 0.0 })
            .build()
            .unwrap();

        let before = world.change_tick();
        let _ = EcsSnapshot::gather(&world);
        assert_eq!(world.change_tick(), before);
    }

    /// A resource declaring a cross-binary shared identity, and an ordinary one
    /// to show the listing distinguishes them.
    #[derive(Debug, Default)]
    struct SharedSettings {
        _value: u32,
    }
    impl crate::resource::Resource for SharedSettings {
        fn shared_name() -> Option<&'static str> {
            Some("diagnostics_test::SharedSettings")
        }
    }

    #[derive(Debug, Default)]
    struct PlainSettings {
        _value: u32,
    }
    impl crate::resource::Resource for PlainSettings {}

    /// Only a shared resource is listed by name, because it is the only kind
    /// whose identity is not already the count line's `TypeId`.
    #[test]
    fn only_shared_resources_are_listed_by_name() {
        let mut world = World::new();
        world.insert_resource(SharedSettings::default());
        world.insert_resource(PlainSettings::default());

        let snapshot = EcsSnapshot::gather(&world);
        assert_eq!(
            snapshot.shared_resources,
            vec!["diagnostics_test::SharedSettings".to_string()]
        );
        assert!(snapshot.render().contains("shared resources"));

        // Dropping the shared resource takes its claim with it, so the section
        // disappears rather than naming a name the world no longer holds.
        world.remove_resource::<SharedSettings>();
        assert!(EcsSnapshot::gather(&world).shared_resources.is_empty());
    }
}

//! The end-to-end test channel: scripted input for the editor's devops
//! suite (plan decision D26, level 3).
//!
//! # Responsibilities
//!
//! - Reads a command file (path in `PILL_EDITOR_TEST_COMMANDS`) that the
//!   suite appends to while the editor runs.
//! - Parses each new line into a [`TestCommand`]: a synthetic click, a key
//!   with a delivery target, a chord (modifiers plus key), a command id to
//!   execute, a tab selection, or a quit request.
//!
//! # Design
//!
//! The file is re-read whole on every poll and lines already consumed are
//! skipped by count, so an append never races a read. The channel is inert
//! unless the environment variable names a file, which keeps a normal run
//! untouched; the executable is a development build, so nothing here is
//! compiled into anything shipped. The suite asserts on the `[editor]`
//! log lines the executable emits when it executes a command, which is also
//! how the input routing decision is observed.

// Standard library
use std::env;
use std::fs;
use std::path::PathBuf;

/// Environment variable naming the command file, appended to by the suite.
const COMMANDS_ENVIRONMENT_VARIABLE: &str = "PILL_EDITOR_TEST_COMMANDS";

/// Environment variable that requests a graceful quit after that many
/// seconds, for the exit-code checks.
pub const SECONDS_ENVIRONMENT_VARIABLE: &str = "PILL_EDITOR_TEST_SECONDS";

// =============================================================================
// TestCommand
// =============================================================================

/// One scripted command from the suite.
#[derive(Clone, Debug, PartialEq)]
pub enum TestCommand {
    /// Click at a point in logical client-area pixels.
    Click {
        /// Distance from the client area's left edge.
        x: f32,
        /// Distance from the client area's top edge.
        y: f32,
    },
    /// Press and release a named key (`escape`, `space`, or one letter),
    /// optionally with a delivery target: `ui` (the default), `scene` (the
    /// editor camera) or `game` (the project).
    Key {
        /// The key's lowercase name, as written in the command file.
        key: String,
        /// Where the key goes; `None` means the UI.
        target: Option<String>,
    },
    /// Select the tab whose title matches, by the shell's selection rule.
    Tab {
        /// The tab title to select.
        title: String,
    },
    /// Focus (or open) a module panel by id, through the shell's panel
    /// path - the same path a restored layout takes for a panel whose
    /// module is not loaded.
    Panel {
        /// The panel id.
        id: String,
    },
    /// Activate a named widget (a button or a field), by synthesizing a
    /// click on its rectangle in the shell.
    Activate {
        /// The widget's name, exactly as the panel called it.
        name: String,
    },
    /// Select an entity by its `NvG` display handle (the scripted
    /// equivalent of a Hierarchy row click).
    Select {
        /// The entity's display handle.
        entity: String,
    },
    /// Type `text` into the focused field, as one committed run.
    Type {
        /// The text to commit.
        text: String,
    },
    /// Scroll the panel area under the last activated widget by `lines`
    /// (positive is up): the scripted wheel.
    Wheel {
        /// Lines to scroll; positive is up.
        lines: f32,
    },
    /// Execute a registered command by id, through the shell's own dispatch.
    Command {
        /// The command id, as registered (`editor.pause_toggle`).
        id: String,
    },
    /// Press a chord in the `ctrl+shift+p` form, modifiers included.
    Chord {
        /// The chord text.
        text: String,
    },
    /// Hover a file over the window (a scripted OS drag, Stage 18).
    HoverFile {
        /// The file's path, as the OS would report it.
        path: String,
    },
    /// Drop a file on the window.
    DropFile {
        /// The file's path, as the OS would report it.
        path: String,
    },
    /// Start an OS drag of one file out of the window (Stage 18). Blocks in
    /// OLE's drag loop until the drag ends, so only a manual checklist or a
    /// deliberate smoke run drives it.
    DragOut {
        /// The file's path: absolute, or relative to the asset root.
        path: String,
    },
    /// Drive one edit through the recorded command path (Stage 15), so the
    /// suite exercises the same route the UI issues.
    Edit(EditCommand),
    /// Ask the world a question; the answer goes to the log.
    Dump(DumpQuery),
    /// Drive one gizmo drag (Stage 20): press on the named axis handle of
    /// the active tool, move by a pixel delta, release - the same pick, drag
    /// and command path the mouse takes.
    GizmoDrag {
        /// The axis index (0 X, 1 Y, 2 Z).
        axis: usize,
        /// Horizontal drag delta, in physical scene pixels.
        dx: f32,
        /// Vertical drag delta, in physical scene pixels.
        dy: f32,
    },
    /// Quit gracefully, so the process exit code is the clean-shutdown one.
    Quit,
    /// A line that did not parse; logged and skipped by the caller.
    Unknown {
        /// The offending line.
        line: String,
    },
}

/// One scripted edit.
#[derive(Clone, Debug, PartialEq)]
pub enum EditCommand {
    /// Write one component field on one entity.
    Set {
        /// The entity's `NvG` display handle.
        entity: String,
        /// Component type name, exact or its `::Name` suffix.
        component: String,
        /// Field name.
        field: String,
        /// The new value, parsed by the caller.
        value: String,
        /// Continuous-edit key, so repeated sets merge into one step.
        coalesce: Option<String>,
    },
    /// Write one element of a fixed-size array field.
    Array {
        /// The entity's `NvG` display handle.
        entity: String,
        /// Component type name, exact or its `::Name` suffix.
        component: String,
        /// Array field name.
        field: String,
        /// Element index.
        index: usize,
        /// The new element value, parsed by the caller.
        value: String,
        /// Continuous-edit key.
        coalesce: Option<String>,
    },
    /// Attach a zero-initialised component.
    Add {
        /// The entity's `NvG` display handle.
        entity: String,
        /// Component type name, exact or its `::Name` suffix.
        component: String,
    },
    /// Remove one component.
    Remove {
        /// The entity's `NvG` display handle.
        entity: String,
        /// Component type name, exact or its `::Name` suffix.
        component: String,
    },
    /// Create an entity carrying one seeded component.
    Create {
        /// Component type name, exact or its `::Name` suffix.
        component: String,
        /// Field values, `field=value` tokens.
        fields: Vec<(String, String)>,
    },
    /// Destroy one entity.
    Destroy {
        /// The entity's `NvG` display handle.
        entity: String,
    },
    /// Toggle one system by registration index (a recorded, undoable edit).
    System {
        /// Registration index from the systems snapshot.
        index: usize,
        /// Desired state.
        enabled: bool,
    },
}

/// One scripted world query.
#[derive(Clone, Debug, PartialEq)]
pub enum DumpQuery {
    /// Whether the entity is alive.
    Entity {
        /// The entity's `NvG` display handle.
        entity: String,
    },
    /// One field's current value.
    Field {
        /// The entity's `NvG` display handle.
        entity: String,
        /// Component type name, exact or its `::Name` suffix.
        component: String,
        /// Field name.
        field: String,
    },
    /// Every registered component with a usable field layout.
    Components,
    /// The live entity list, first few with their component names.
    Entities,
}

// =============================================================================
// TestChannel
// =============================================================================

/// The command file reader.
pub struct TestChannel {
    /// The file the suite appends commands to.
    path: PathBuf,
    /// How many lines have already been executed.
    consumed: usize,
}

impl TestChannel {
    /// The channel, when `PILL_EDITOR_TEST_COMMANDS` names a readable file.
    pub fn from_environment() -> Option<TestChannel> {
        let path = PathBuf::from(env::var_os(COMMANDS_ENVIRONMENT_VARIABLE)?);
        // A missing file is the suite's job to create; refuse quietly but
        // visibly rather than failing the editor's startup.
        if !path.is_file() {
            return None;
        }
        Some(TestChannel { path, consumed: 0 })
    }

    /// The commands appended since the last poll, in order.
    pub fn poll(&mut self) -> Vec<TestCommand> {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut commands = Vec::new();
        for line in text.lines().skip(self.consumed) {
            self.consumed += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            commands.push(parse_command(trimmed));
        }
        commands
    }
}

/// Parse one command line.
fn parse_command(line: &str) -> TestCommand {
    // `type` and `activate` carry arbitrary text, so they take the rest of
    // the line (widget names are panel labels, which contain spaces).
    if let Some(text) = line.strip_prefix("type ") {
        return TestCommand::Type {
            text: text.to_string(),
        };
    }
    if let Some(name) = line.strip_prefix("activate ") {
        return TestCommand::Activate {
            name: name.to_string(),
        };
    }
    // Paths contain spaces, so the file-drop verbs take the rest of the line.
    if let Some(path) = line.strip_prefix("hover-file ") {
        return TestCommand::HoverFile {
            path: path.to_string(),
        };
    }
    if let Some(path) = line.strip_prefix("drop-file ") {
        return TestCommand::DropFile {
            path: path.to_string(),
        };
    }
    if let Some(path) = line.strip_prefix("drag-out ") {
        return TestCommand::DragOut {
            path: path.to_string(),
        };
    }
    if let Some(rest) = line.strip_prefix("wheel ") {
        return match rest.trim().parse::<f32>() {
            Ok(lines) => TestCommand::Wheel { lines },
            Err(_) => TestCommand::Unknown {
                line: line.to_string(),
            },
        };
    }
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("click") => {
            let point = (|| {
                let x: f32 = parts.next()?.parse().ok()?;
                let y: f32 = parts.next()?.parse().ok()?;
                Some((x, y))
            })();
            match point {
                Some((x, y)) => TestCommand::Click { x, y },
                None => TestCommand::Unknown {
                    line: line.to_string(),
                },
            }
        }
        Some("key") => match parts.next() {
            Some(key) => TestCommand::Key {
                key: key.to_lowercase(),
                target: parts.next().map(str::to_lowercase),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("tab") => match parts.next() {
            Some(title) => TestCommand::Tab {
                title: title.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("panel") => match parts.next() {
            Some(id) => TestCommand::Panel { id: id.to_string() },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("activate") => match parts.next() {
            Some(name) => TestCommand::Activate {
                name: name.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("select") => match parts.next() {
            Some(entity) => TestCommand::Select {
                entity: entity.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("command") => match parts.next() {
            Some(id) => TestCommand::Command { id: id.to_string() },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("chord") => match parts.next() {
            Some(text) => TestCommand::Chord {
                text: text.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("quit") => TestCommand::Quit,
        Some("edit") => parse_edit(&mut parts, line),
        Some("dump") => parse_dump(&mut parts, line),
        Some("gizmo") => parse_gizmo(&mut parts, line),
        _ => TestCommand::Unknown {
            line: line.to_string(),
        },
    }
}

/// Parse `gizmo drag <axis> <dx> <dy>`.
fn parse_gizmo(parts: &mut std::str::SplitWhitespace<'_>, line: &str) -> TestCommand {
    let unknown = || TestCommand::Unknown {
        line: line.to_string(),
    };
    let Some(verb) = parts.next() else {
        return unknown();
    };
    if verb != "drag" {
        return unknown();
    }
    let (Some(axis), Some(dx), Some(dy)) = (parts.next(), parts.next(), parts.next()) else {
        return unknown();
    };
    let (Ok(axis), Ok(dx), Ok(dy)) = (axis.parse::<usize>(), dx.parse::<f32>(), dy.parse::<f32>())
    else {
        return unknown();
    };
    TestCommand::GizmoDrag { axis, dx, dy }
}

/// Parse `edit <verb> ...`.
fn parse_edit(parts: &mut std::str::SplitWhitespace<'_>, line: &str) -> TestCommand {
    let unknown = || TestCommand::Unknown {
        line: line.to_string(),
    };
    let Some(verb) = parts.next() else {
        return unknown();
    };
    // Creation names the component first (there is no entity yet); every
    // other verb addresses an entity and then a component.
    if verb == "create" {
        let Some(component) = parts.next() else {
            return unknown();
        };
        let mut fields = Vec::new();
        for token in parts {
            let Some((field, value)) = token.split_once('=') else {
                return unknown();
            };
            fields.push((field.to_string(), value.to_string()));
        }
        return TestCommand::Edit(EditCommand::Create {
            component: component.to_string(),
            fields,
        });
    }
    // Destroying names only the entity.
    if verb == "destroy" {
        let Some(entity) = parts.next() else {
            return unknown();
        };
        return TestCommand::Edit(EditCommand::Destroy {
            entity: entity.to_string(),
        });
    }
    // A system toggle names an index, not an entity.
    if verb == "system" {
        let (Some(index), Some(state)) = (parts.next(), parts.next()) else {
            return unknown();
        };
        let Ok(index) = index.parse::<usize>() else {
            return unknown();
        };
        let enabled = match state {
            "on" => true,
            "off" => false,
            _ => return unknown(),
        };
        return TestCommand::Edit(EditCommand::System { index, enabled });
    }
    let (Some(entity), Some(component)) = (parts.next(), parts.next()) else {
        return unknown();
    };
    let entity = entity.to_string();
    let component = component.to_string();
    match verb {
        "set" => {
            let (Some(field), Some(value)) = (parts.next(), parts.next()) else {
                return unknown();
            };
            let coalesce = parse_coalesce(parts);
            TestCommand::Edit(EditCommand::Set {
                entity,
                component,
                field: field.to_string(),
                value: value.to_string(),
                coalesce,
            })
        }
        "array" => {
            let (Some(field), Some(index), Some(value)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return unknown();
            };
            let Ok(index) = index.parse::<usize>() else {
                return unknown();
            };
            let coalesce = parse_coalesce(parts);
            TestCommand::Edit(EditCommand::Array {
                entity,
                component,
                field: field.to_string(),
                index,
                value: value.to_string(),
                coalesce,
            })
        }
        "add" => TestCommand::Edit(EditCommand::Add { entity, component }),
        "remove" => TestCommand::Edit(EditCommand::Remove { entity, component }),
        _ => unknown(),
    }
}

/// The optional trailing `coalesce <key>` of an edit.
fn parse_coalesce(parts: &mut std::str::SplitWhitespace<'_>) -> Option<String> {
    match parts.next() {
        Some("coalesce") => parts.next().map(str::to_string),
        _ => None,
    }
}

/// Parse `dump <kind> ...`.
fn parse_dump(parts: &mut std::str::SplitWhitespace<'_>, line: &str) -> TestCommand {
    let unknown = || TestCommand::Unknown {
        line: line.to_string(),
    };
    match parts.next() {
        Some("components") => TestCommand::Dump(DumpQuery::Components),
        Some("entities") => TestCommand::Dump(DumpQuery::Entities),
        Some("entity") => match parts.next() {
            Some(entity) => TestCommand::Dump(DumpQuery::Entity {
                entity: entity.to_string(),
            }),
            None => unknown(),
        },
        Some("field") => match (parts.next(), parts.next(), parts.next()) {
            (Some(entity), Some(component), Some(field)) => TestCommand::Dump(DumpQuery::Field {
                entity: entity.to_string(),
                component: component.to_string(),
                field: field.to_string(),
            }),
            _ => unknown(),
        },
        _ => unknown(),
    }
}

/// The seconds the environment asked the editor to run for, if any.
pub fn seconds_from_environment() -> Option<f32> {
    env::var(SECONDS_ENVIRONMENT_VARIABLE)
        .ok()
        .and_then(|value| value.parse().ok())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_into_commands() {
        assert_eq!(
            parse_command("click 100 250.5"),
            TestCommand::Click { x: 100.0, y: 250.5 }
        );
        assert_eq!(
            parse_command("key Escape"),
            TestCommand::Key {
                key: "escape".to_string(),
                target: None,
            }
        );
        assert_eq!(
            parse_command("key W Scene"),
            TestCommand::Key {
                key: "w".to_string(),
                target: Some("scene".to_string()),
            }
        );
        assert_eq!(
            parse_command("tab Game"),
            TestCommand::Tab {
                title: "Game".to_string()
            }
        );
        assert_eq!(
            parse_command("command editor.pause_toggle"),
            TestCommand::Command {
                id: "editor.pause_toggle".to_string()
            }
        );
        assert_eq!(
            parse_command("chord ctrl+shift+p"),
            TestCommand::Chord {
                text: "ctrl+shift+p".to_string()
            }
        );
        assert_eq!(parse_command("quit"), TestCommand::Quit);
        assert!(matches!(
            parse_command("click nope"),
            TestCommand::Unknown { .. }
        ));
        assert!(matches!(
            parse_command("dance"),
            TestCommand::Unknown { .. }
        ));
    }

    #[test]
    fn edit_and_dump_lines_parse() {
        assert_eq!(
            parse_command("edit set 2v0 PhysicsState position_x 4.5"),
            TestCommand::Edit(EditCommand::Set {
                entity: "2v0".to_string(),
                component: "PhysicsState".to_string(),
                field: "position_x".to_string(),
                value: "4.5".to_string(),
                coalesce: None,
            })
        );
        assert_eq!(
            parse_command("edit set 2v0 PhysicsState position_x 7 coalesce slider"),
            TestCommand::Edit(EditCommand::Set {
                entity: "2v0".to_string(),
                component: "PhysicsState".to_string(),
                field: "position_x".to_string(),
                value: "7".to_string(),
                coalesce: Some("slider".to_string()),
            })
        );
        assert_eq!(
            parse_command("edit array 2v0 Wave values 3 -1"),
            TestCommand::Edit(EditCommand::Array {
                entity: "2v0".to_string(),
                component: "Wave".to_string(),
                field: "values".to_string(),
                index: 3,
                value: "-1".to_string(),
                coalesce: None,
            })
        );
        assert_eq!(
            parse_command("edit add 2v0 SplineSample"),
            TestCommand::Edit(EditCommand::Add {
                entity: "2v0".to_string(),
                component: "SplineSample".to_string(),
            })
        );
        assert_eq!(
            parse_command("edit remove 2v0 SplineSample"),
            TestCommand::Edit(EditCommand::Remove {
                entity: "2v0".to_string(),
                component: "SplineSample".to_string(),
            })
        );
        assert_eq!(
            parse_command("edit create PhysicsState position_x=1.5 radius=0.25"),
            TestCommand::Edit(EditCommand::Create {
                component: "PhysicsState".to_string(),
                fields: vec![
                    ("position_x".to_string(), "1.5".to_string()),
                    ("radius".to_string(), "0.25".to_string()),
                ],
            })
        );
        assert_eq!(
            parse_command("dump entity 2v0"),
            TestCommand::Dump(DumpQuery::Entity {
                entity: "2v0".to_string(),
            })
        );
        assert_eq!(
            parse_command("dump field 2v0 PhysicsState position_x"),
            TestCommand::Dump(DumpQuery::Field {
                entity: "2v0".to_string(),
                component: "PhysicsState".to_string(),
                field: "position_x".to_string(),
            })
        );
        assert_eq!(
            parse_command("dump components"),
            TestCommand::Dump(DumpQuery::Components)
        );
        assert_eq!(
            parse_command("dump entities"),
            TestCommand::Dump(DumpQuery::Entities)
        );
        assert_eq!(
            parse_command("edit destroy 3v1"),
            TestCommand::Edit(EditCommand::Destroy {
                entity: "3v1".to_string(),
            })
        );
        assert!(matches!(
            parse_command("edit set 2v0"),
            TestCommand::Unknown { .. }
        ));
        assert!(matches!(
            parse_command("edit array 2v0 Wave values x 1"),
            TestCommand::Unknown { .. }
        ));
        assert!(matches!(
            parse_command("edit create PhysicsState nope"),
            TestCommand::Unknown { .. }
        ));
        assert!(matches!(
            parse_command("dump field 2v0 PhysicsState"),
            TestCommand::Unknown { .. }
        ));
    }
}

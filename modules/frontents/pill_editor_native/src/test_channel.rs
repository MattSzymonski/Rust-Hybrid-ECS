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
    /// Move the pointer without pressing anything (Stage 22): the suite
    /// hovers a button to prove the UI redraws on input.
    Move {
        /// Distance from the client area's left edge.
        x: f32,
        /// Distance from the client area's top edge.
        y: f32,
    },
    /// Right-click at a point in logical client-area pixels: the gesture
    /// that asks the shell for a context menu (Stage 21).
    RightClick {
        /// Distance from the client area's left edge.
        x: f32,
        /// Distance from the client area's top edge.
        y: f32,
    },
    /// Deliver a pointer to an open popup, in the popup's own logical
    /// pixels, with `release` for the left button coming up (Stage 21).
    PopupPointer {
        /// The popup's id, as the shell named it.
        id: String,
        /// Distance from the popup's left edge.
        x: f32,
        /// Distance from the popup's top edge.
        y: f32,
        /// Whether the left button came up there.
        released: bool,
    },
    /// Dismiss a popup the way the platform does (Stage 21): the state an
    /// outside click, a lost focus or an Escape leaves behind. A scripted
    /// run cannot move the real focus, so this is the platform's half.
    PopupDismiss {
        /// The popup's id, as the shell named it.
        id: String,
    },
    /// Deliver one pointer event to a free-standing window (Stage 24), in
    /// the window's own logical pixels.
    WindowPointer {
        /// The window's id, as the shell named it.
        id: String,
        /// Distance from the window's left edge.
        x: f32,
        /// Distance from the window's top edge.
        y: f32,
        /// What happened: `press`, `release`, or a bare move.
        kind: String,
    },
    /// Press Enter or Escape in a free-standing window (Stage 24): the keys
    /// a modal answers.
    WindowKey {
        /// The window's id, as the shell named it.
        id: String,
        /// `enter` or `escape`.
        key: String,
    },
    /// Close a window the way its title bar's close button does (Stage 24);
    /// a scripted run cannot click it, so this is the platform's half.
    WindowDismiss {
        /// The window's id, as the shell named it.
        id: String,
    },
    /// Type into a free-standing window (Stage 25): the rest of the line is
    /// the text a focused field in that window receives.
    WindowType {
        /// The window's id, as the shell named it.
        id: String,
        /// The text to commit.
        text: String,
    },
    /// Rebuild the asset listing on the next frame (Stage 25): a suite that
    /// writes files from outside the editor needs the rows - and the meta
    /// state they carry - to catch up at once instead of at the next
    /// periodic walk.
    RefreshAssets,
    /// Move the editor window's outer position, in physical pixels
    /// (Stage 21: a popup flips at the monitor's work area, which the suite
    /// reaches by moving the window to a monitor edge).
    WindowMove {
        /// Physical x of the window's outer top-left.
        x: i32,
        /// Physical y of the window's outer top-left.
        y: i32,
    },
    /// Move one free-standing window's outer top-left, in logical pixels
    /// (Stage 29: the arrangement a restart has to remember).
    WindowPlace {
        /// The window's id, as the shell named it.
        id: String,
        /// Logical x of the window's outer top-left.
        x: f32,
        /// Logical y of the window's outer top-left.
        y: f32,
    },
    /// Resize one free-standing window's client area, in logical pixels
    /// (Stage 29).
    WindowResize {
        /// The window's id, as the shell named it.
        id: String,
        /// Logical width.
        width: f32,
        /// Logical height.
        height: f32,
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
    /// Select an asset by its path (Stage 25), the scripted equivalent of
    /// an Assets row click; recorded like every other selection change.
    SelectAsset {
        /// The asset's path relative to the asset root.
        path: String,
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
    /// and command path the mouse takes. `view` names the scene view the
    /// drag works on (Stage 29; the primary view when absent).
    GizmoDrag {
        /// The axis index (0 X, 1 Y, 2 Z).
        axis: usize,
        /// Horizontal drag delta, in physical scene pixels.
        dx: f32,
        /// Vertical drag delta, in physical scene pixels.
        dy: f32,
        /// The scene view's tab node, when the drag targets a later view.
        view: Option<u64>,
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
    /// Create a standalone asset (Stage 25): the recorded, undoable file
    /// edit the Assets panel's create dialog issues.
    AssetCreate {
        /// The registered standalone type's name.
        type_name: String,
        /// The destination folder, relative to the asset root; empty for
        /// the root.
        folder: String,
        /// The name to create.
        name: String,
    },
    /// Delete one asset file and its `.meta` sidecar (Stage 25).
    AssetDelete {
        /// The asset's path relative to the asset root.
        path: String,
    },
    /// Move or rename one asset file and its `.meta` sidecar (Stage 25).
    AssetMove {
        /// The current path, relative to the asset root.
        from: String,
        /// The new path, relative to the asset root.
        to: String,
    },
    /// Write an asset's missing `.meta` sidecar (Stage 25).
    AssetMeta {
        /// The asset's path relative to the asset root.
        path: String,
    },
    /// Write one resource field through the recorded command path
    /// (Stage 25).
    ResourceSet {
        /// The resource's stable name.
        resource: String,
        /// The field's name within the resource.
        field: String,
        /// The new value, in the scripted scalar syntax.
        value: String,
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
    /// One resource field's current value (Stage 25).
    Resource {
        /// The resource's stable name.
        resource: String,
        /// The field's name within the resource.
        field: String,
    },
    /// The shared selection, entity and asset (Stage 25).
    Selection,
    /// Every live standalone asset type (Stage 25): the create dialog's
    /// rows, and the names a suite uses with `edit asset create`.
    StandaloneTypes,
    /// How many window-surface UI frames the editor has presented (Stage
    /// 22): the idle check counts it across a quiet stretch.
    UiFrames,
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
        Some("move") => {
            let point = (|| {
                let x: f32 = parts.next()?.parse().ok()?;
                let y: f32 = parts.next()?.parse().ok()?;
                Some((x, y))
            })();
            match point {
                Some((x, y)) => TestCommand::Move { x, y },
                None => TestCommand::Unknown {
                    line: line.to_string(),
                },
            }
        }
        Some("rightclick") => {
            let point = (|| {
                let x: f32 = parts.next()?.parse().ok()?;
                let y: f32 = parts.next()?.parse().ok()?;
                Some((x, y))
            })();
            match point {
                Some((x, y)) => TestCommand::RightClick { x, y },
                None => TestCommand::Unknown {
                    line: line.to_string(),
                },
            }
        }
        Some("popup-pointer") => {
            let (Some(id), Some(x), Some(y)) = (parts.next(), parts.next(), parts.next()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            TestCommand::PopupPointer {
                id: id.to_string(),
                x,
                y,
                released: parts.next() == Some("release"),
            }
        }
        Some("popup-dismiss") => match parts.next() {
            Some(id) => TestCommand::PopupDismiss { id: id.to_string() },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("window-pointer") => {
            let (Some(id), Some(x), Some(y)) = (parts.next(), parts.next(), parts.next()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            TestCommand::WindowPointer {
                id: id.to_string(),
                x,
                y,
                kind: parts.next().unwrap_or("move").to_string(),
            }
        }
        Some("window-key") => match (parts.next(), parts.next()) {
            (Some(id), Some(key)) => TestCommand::WindowKey {
                id: id.to_string(),
                key: key.to_lowercase(),
            },
            _ => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("window-dismiss") => match parts.next() {
            Some(id) => TestCommand::WindowDismiss { id: id.to_string() },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("window-type") => {
            // The rest of the line is the text, spaces included.
            let rest = line.trim_start();
            let rest = rest
                .strip_prefix("window-type")
                .unwrap_or(rest)
                .trim_start();
            let rest = rest.trim_end();
            match rest.split_once(char::is_whitespace) {
                Some((id, text)) if !text.trim().is_empty() => TestCommand::WindowType {
                    id: id.to_string(),
                    text: text.trim().to_string(),
                },
                _ => TestCommand::Unknown {
                    line: line.to_string(),
                },
            }
        }
        Some("window-move") => {
            let (Some(x), Some(y)) = (parts.next(), parts.next()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            let (Ok(x), Ok(y)) = (x.parse::<i32>(), y.parse::<i32>()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            TestCommand::WindowMove { x, y }
        }
        Some("window-place") => {
            let (Some(id), Some(x), Some(y)) = (parts.next(), parts.next(), parts.next()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            TestCommand::WindowPlace {
                id: id.to_string(),
                x,
                y,
            }
        }
        Some("window-resize") => {
            let (Some(id), Some(width), Some(height)) = (parts.next(), parts.next(), parts.next())
            else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            let (Ok(width), Ok(height)) = (width.parse::<f32>(), height.parse::<f32>()) else {
                return TestCommand::Unknown {
                    line: line.to_string(),
                };
            };
            TestCommand::WindowResize {
                id: id.to_string(),
                width,
                height,
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
        Some("activate") => {
            // The rest of the line is the widget's name: a widget's name is
            // its label, spaces included (Stage 24 made window widgets
            // addressable too, and a modal button is named through its
            // window).
            let rest = line.trim_start();
            let rest = rest.strip_prefix("activate").unwrap_or(rest).trim_start();
            if rest.is_empty() {
                TestCommand::Unknown {
                    line: line.to_string(),
                }
            } else {
                TestCommand::Activate {
                    name: rest.trim_end().to_string(),
                }
            }
        }
        Some("select") => match parts.next() {
            Some(entity) => TestCommand::Select {
                entity: entity.to_string(),
            },
            None => TestCommand::Unknown {
                line: line.to_string(),
            },
        },
        Some("select-asset") => match parts.next() {
            Some(path) => TestCommand::SelectAsset {
                path: path.to_string(),
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
        Some("refresh-assets") => TestCommand::RefreshAssets,
        Some("edit") => parse_edit(&mut parts, line),
        Some("dump") => parse_dump(&mut parts, line),
        Some("gizmo") => parse_gizmo(&mut parts, line),
        _ => TestCommand::Unknown {
            line: line.to_string(),
        },
    }
}

/// Parse `gizmo drag <axis> <dx> <dy> [view:<node>]`.
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
    let view = match parts.next() {
        Some(token) => match token
            .strip_prefix("view:")
            .and_then(|node| node.parse::<u64>().ok())
        {
            Some(node) => Some(node),
            None => return unknown(),
        },
        None => None,
    };
    TestCommand::GizmoDrag { axis, dx, dy, view }
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
    // The asset commands (Stage 25) name files, not entities; `create`
    // names the type, the folder (`-` for the asset root) and the name.
    if verb == "asset" {
        let Some(action) = parts.next() else {
            return unknown();
        };
        return match action {
            "create" => {
                let (Some(type_name), Some(folder), Some(name)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return unknown();
                };
                let folder = if folder == "-" {
                    String::new()
                } else {
                    folder.to_string()
                };
                TestCommand::Edit(EditCommand::AssetCreate {
                    type_name: type_name.to_string(),
                    folder,
                    name: name.to_string(),
                })
            }
            "delete" => match parts.next() {
                Some(path) => TestCommand::Edit(EditCommand::AssetDelete {
                    path: path.to_string(),
                }),
                None => unknown(),
            },
            "move" => match (parts.next(), parts.next()) {
                (Some(from), Some(to)) => TestCommand::Edit(EditCommand::AssetMove {
                    from: from.to_string(),
                    to: to.to_string(),
                }),
                _ => unknown(),
            },
            "meta" => match parts.next() {
                Some(path) => TestCommand::Edit(EditCommand::AssetMeta {
                    path: path.to_string(),
                }),
                None => unknown(),
            },
            _ => unknown(),
        };
    }
    // The resource command (Stage 25) names a resource and a field, never
    // an entity.
    if verb == "resource" {
        let (Some(action), Some(resource), Some(field), Some(value)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return unknown();
        };
        if action != "set" {
            return unknown();
        }
        return TestCommand::Edit(EditCommand::ResourceSet {
            resource: resource.to_string(),
            field: field.to_string(),
            value: value.to_string(),
        });
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
        Some("ui-frames") => TestCommand::Dump(DumpQuery::UiFrames),
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
        Some("resource") => match (parts.next(), parts.next()) {
            (Some(resource), Some(field)) => TestCommand::Dump(DumpQuery::Resource {
                resource: resource.to_string(),
                field: field.to_string(),
            }),
            _ => unknown(),
        },
        Some("selection") => TestCommand::Dump(DumpQuery::Selection),
        Some("standalone-types") => TestCommand::Dump(DumpQuery::StandaloneTypes),
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
        assert_eq!(parse_command("refresh-assets"), TestCommand::RefreshAssets);
        assert_eq!(
            parse_command("rightclick 1240 300"),
            TestCommand::RightClick {
                x: 1240.0,
                y: 300.0
            }
        );
        assert_eq!(
            parse_command("popup-pointer menu 10 34 release"),
            TestCommand::PopupPointer {
                id: "menu".to_string(),
                x: 10.0,
                y: 34.0,
                released: true,
            }
        );
        assert_eq!(
            parse_command("popup-dismiss context"),
            TestCommand::PopupDismiss {
                id: "context".to_string()
            }
        );
        assert_eq!(
            parse_command("window-move 1900 40"),
            TestCommand::WindowMove { x: 1900, y: 40 }
        );
        assert_eq!(
            parse_command("move 40 13"),
            TestCommand::Move { x: 40.0, y: 13.0 }
        );
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
    fn dump_lines_parse() {
        assert_eq!(
            parse_command("dump entities"),
            TestCommand::Dump(DumpQuery::Entities)
        );
        assert_eq!(
            parse_command("dump ui-frames"),
            TestCommand::Dump(DumpQuery::UiFrames)
        );
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

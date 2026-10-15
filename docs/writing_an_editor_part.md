# Writing an editor part

An **editor part** is a crate that gives an extension a place in the native
editor: panels in the dock, entries in the menus and command palette,
shortcuts, undoable edits, tool windows and dialogs, and drawing through the
engine's debug contract. Editor parts run only in the editor; a game,
shipping or web build never compiles or loads one.

The worked example is
`modules/extensions/pill_spline/pill_spline_editor/` - every mechanism this
guide describes is used there in about three hundred lines. The panel tree
API is `pill_ui`; its CSS subset is documented in
`modules/pill_ui/docs/css_subset.md`.

## 1. Where an editor part lives

An extension's folder holds its runtime crates and its editor crate:

```
modules/extensions/<extension>/
    <extension>/            the runtime crate (world systems, components)
    <extension>_editor/     the editor part (this guide)
    generated/              host-generated wrappers (never edit)
        host_module_<extension>/
        host_module_<extension>_editor/
```

Rules the host enforces by convention, not configuration:

- The editor crate is discovered by its **name**: inside the folder
  `<extension>`, the crate `<extension>_editor` is the folder's editor part.
  No list in `project_settings.yaml` mentions it.
- An **editor host** loads the editor crate after the folder's runtime
  crates, and reloads it whenever one of them reloads. No other build ever
  builds or loads it.
- The **wrapper crate** under `generated/` is written by the host on
  startup. It is the `cdylib` the host actually loads; do not edit it, do
  not add it by hand.
- Modules must live inside this workspace: the workspace lockfile is what
  makes the module and the host resolve one identical dependency graph.

A minimal editor crate is two files.

`modules/extensions/<extension>/<extension>_editor/Cargo.toml`:

```toml
[package]
name = "<extension>_editor"
version = "0.1.0"
edition = "2021"

[lints]
workspace = true

[lib]
# Named after the crate rather than `lib.rs`, like every extension crate:
# the Windows debugger falls back to file names, and every crate root is
# `src/lib.rs` unless you name it. `pill_hot_scan` reads this key.
path = "src/<extension>_editor.rs"
# `rlib` only: the host loads the generated wrapper, which links this crate.
crate-type = ["rlib"]

[dependencies]
pill_core = { path = "../../../pill_core" }
pill_editor_api = { path = "../../../hosting/pill_editor_api" }
pill_editor_core = { path = "../../../frontents/pill_editor_core" }
pill_engine = { path = "../../../pill_engine" }
pill_ui = { path = "../../../pill_ui" }
# Only when a persistable resource (section 7) uses derives.
serde = { version = "1", features = ["derive"] }
# Depend on the runtime crate for its types; `default-features = false`
# keeps feature-gated test hooks out of the editor build.
<extension> = { path = "../<extension>", default-features = false }
```

`modules/extensions/<extension>/<extension>_editor/src/<extension>_editor.rs`:

```rust
//! The <extension> extension's editor part.

// External crates
use pill_engine::{pill_module, Engine};

/// Registers the editor crate against the host engine.
/// Returns zero on success.
///
/// Public so the generated `host_module_<extension>_editor` wrapper can
/// carry it as `pill_module_init`.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    // Panels, commands and systems go here; see the sections below.
    let _ = engine;
    0
}
```

The workspace manifest globs `extensions/*/pill_*`, so a new crate under
that shape is a workspace member automatically.

## 2. Panels

A panel is a **function** the shell calls every frame it is visible:

```rust
use pill_editor_api::panels::{register_editor_panels, EditorPanelSpec, PanelContext};
use pill_ui::panel::PanelBuilder;

pub fn my_panel(builder: &mut PanelBuilder<'_>, context: &mut PanelContext<'_>) {
    builder.heading("My Tool");
    builder.separator();
    builder.label(&format!("{} entities", context.snapshot.entities.len()));
    if builder.button("Do The Thing") {
        context.work.command("my_extension.do_thing");
    }
}
```

Register the panel's whole set in one call from `register`:

```rust
register_editor_panels(
    engine,
    "<extension>_editor",     // the owner: every generation replaces this set
    &[
        EditorPanelSpec::new("my_tool", "My Tool", my_panel),
        EditorPanelSpec::new("my_dialog", "My Dialog", my_dialog_panel),
    ],
);
```

- The **id** is what layouts, tool windows, command tooling and the scripted
  test channel address (`panel my_tool`).
- The **title** is the tab's label.
- Registering as one set means a reload replaces exactly this set: a panel a
  later generation drops retires with it, and a tab saved for it shows a
  placeholder until the crate returns.

Inside a panel, `context` gives you the frame's data and the work channel:

| You want | Use |
| --- | --- |
| Entities, systems, pause state, assets | `context.snapshot` fields |
| The engine/game log ring | `context.logs` |
| The shared selection | `context.selection` |
| The panel's rectangle in logical pixels | `context.rect` |
| Persistent per-widget text buffers | `context.text_buffers` (key by a stable string) |
| Queue a command | `context.work.command("id")` |
| Ask for a selection change | `context.work.select_entity(...)`, `select_asset(...)` |
| Open a window, close one | see section 5 |

Panel content is a **plain-data tree** built fresh every frame through
`PanelBuilder`: labels, headings, rows, columns, buttons, checkboxes,
sliders, drag values, text fields, combo boxes, color and vector fields,
collapsing sections, scroll areas, tables, list rows and trees (see
`pill_ui::panel::PanelBuilder` for the full list). The shell never keeps a
function pointer or a borrowed string from a panel across a frame (section
7).

## 3. Commands and shortcuts

One command is one id, one title, and any number of bindings:

```rust
use pill_editor_core::commands::{CommandSpec, KeymapPreset};

if let Some(registry) = world.get_resource_mut::<CommandRegistry>() {
    registry.register(
        "<extension>_editor",                  // owner: reload replaces this set
        vec![
            CommandSpec::new("my_extension.do_thing", "Do The Thing")
                .menu(&["My Extension", "Do The Thing"])
                .bind(KeymapPreset::Blender, "ctrl+j")
                .bind(KeymapPreset::Unity, "ctrl+j"),
        ],
    );
}
```

- A registered command shows in the **menu** you name and in the **command
  palette**; shortcuts come from the keymap presets plus single-binding
  overrides in the user's keybindings file.
- The shell **dispatches execution, not behavior**: it records the id in
  `CommandInbox`. Your crate drains the ids it owns in a system it
  registers, and does the work there:

```rust
use pill_editor_core::commands::CommandInbox;
use pill_engine::{pill_hot_fn, Query, ResMut};

#[pill_hot_fn]
pub fn my_commands(mut inbox: ResMut<CommandInbox>) {
    let Some(mut inbox) = inbox.get_mut() else { return };
    let owned: Vec<String> = inbox
        .executed
        .iter()
        .filter(|id| id.starts_with("my_extension."))
        .cloned()
        .collect();
    for id in owned {
        inbox.executed.retain(|candidate| candidate != &id);
        match id.as_str() {
            "my_extension.do_thing" => { /* do it, or queue a recorded edit */ }
            _ => {}
        }
    }
}
```

Register the system with `engine.register_system("<extension>_editor_commands", my_commands)`
from `register`. The shell cannot call into a module's code after a reload;
the inbox is the only door, and it is one-way.

## 4. Undoable edits

Never mutate the world directly from a panel or a command handler if the
edit should be undoable. Queue a **recorded command** and let the
executable apply it at the frame boundary - the boundary is where the
inverse is computed, so the edit and its undo are one step on the shared
history:

```rust
use pill_editor_core::editor_state::EditorCommand;
use pill_editor_core::commands::PendingEditorCommands;
use pill_engine::{FieldValue, pill_hot_fn, Query, Entity};

#[pill_hot_fn]
pub fn bump(mut pending: ResMut<PendingEditorCommands>, mut entities: Query<Entity>) {
    let Some(mut pending) = pending.get_mut() else { return };
    let Some((entity,)) = entities.iter_mut().next() else { return };
    pending.push_recorded(
        vec![EditorCommand::SetField {
            entity,
            component: std::any::type_name::<MyComponent>().to_string(),
            field: "value".to_string(),
            value: FieldValue::F32(1.0),
        }],
        "my_extension.do_thing",     // the history entry's label
        None,                        // a coalesce key for sliders and drags
    );
}
```

- The recorded command is the **only writer**: queue it, do not also write
  the field, or the inverse sees the post-edit value.
- Anything the history keeps is undoable through the editor's Undo/Redo,
  across reloads and alongside every other edit.
- Direct world mutations (writing through a `Query`) are allowed for
  non-undoable tool state, and are exactly what the undo section of the
  plan calls "not recorded". Selection changes through
  `context.work.select_entity` go through the recorded path automatically.

## 5. Tool windows and dialogs

A panel can move out of the dock: the shell owns free-standing windows, and
the request names a **registered panel** - never a closure:

```rust
if builder.button_named("my.open_window", "Detach Tool Window") {
    context.work.open_tool_window(
        "my_tool_window",        // the window's stable id
        "My Tool",               // the title bar text
        320.0, 240.0,            // logical size
        "my_tool",               // the registered panel it draws
    );
}
if builder.button_named("my.confirm", "Do The Thing (confirm)") {
    context.work.open_modal(
        "my_confirm",
        "Do The Thing",
        380.0, 170.0,
        "my_dialog",
        vec![
            EditorWindowButton { label: "Do It".into(), command: "my_extension.do_thing".into() },
            EditorWindowButton { label: "Cancel".into(), command: String::new() },
        ],
    );
}
```

- The first modal button is the default: Enter answers it; Escape dismisses
  the modal without running anything.
- `context.work.close_window("id")` closes one this panel opened; the title
  bar's close button works too, and the panel layer hears about it the same
  way.
- Windows resolve through the panel registry every frame: a reload reopens a
  tool window on the new generation and closes a modal (its answer belonged
  to the code that asked). A window whose panel dies with its crate closes
  with it.
- A docking tab also pops out from the editor side
  (`editor.popout_tab`); its window draws the same registered panel.

## 6. Debug draw and gizmos

The frame's debug contract is the `DebugDraw` resource: lines with a style,
cleared every frame (OneFrame lifetime), visible in the scene views your
editor shows.

```rust
use pill_engine::{DebugDraw, DebugStyle};

for (_, transform) in transforms.iter_mut() {
    debug.line([x0, y0, z0], [x1, y1, z1], DebugStyle::default());
}
```

- The renderer's selection outline is the world resource
  `pill_renderer_api::picking::SelectionOutlines`; the executable owns it.
- Gizmo geometry and math (handles, rays, snapping) live in
  `pill_editor_core::gizmo` for tools that need their own manipulators; the
  editor's own transform gizmo reads the shared `GizmoState` resource.
- The Scene view's camera belongs to the editor host (one per scene view);
  an editor part never owns a camera.

## 7. State, reload and the cross-DLL rules

Editor code lives in its own `cdylib` next to the editor, the other modules
and the project. The rules that keep that safe are not optional:

1. **Panels are plain data, rebuilt every frame.** A `PanelBuilder` emits a
   tree and draw calls for one frame; the shell keeps none of it, and no
   pointer or borrowed string crosses the boundary. State that must survive
   a frame goes into a world resource or the panel's `text_buffers`.
2. **Your state lives in world resources with a stable name.** For state
   that must survive your own reload, register a **persistable** resource:
   derive `Serialize, Deserialize`, implement
   `Resource::shared_name() -> Some("<crate>::<Type>")` (the name is what
   carries the value across generations) and register it with
   `world.register_persistable_resource::<T>()`.
3. **Registrations are keyed by owner.** Panels and commands registered
   under a name are replaced as a set on the next generation's `register`:
   never register per item, and never assume last generation's
   registrations are still there.
4. **The command inbox is the only call channel.** Panels and the shell
   cannot invoke your code across a reload; ids and resources are the
   interface.
5. **The runtime crate is the world's owner.** An editor part may read the
   world and queue recorded commands; systems that define game behavior
   belong in the runtime crate, where a game build can see them.

## 8. Live editing your editor part

- The crate is built and loaded by the editor host; saving a source file
  under its `src/` reloads only this crate, after the runtime crate it
  links. Panels, commands and windows survive the way section 5 and 7
  describe.
- Mark a function `#[pill_hot_fn]` (panel bodies, command handlers) so a
  body-only edit **live-patches** in place instead of reloading the crate:
  the running frame keeps its state and the function body swaps.
- Put `#[pill_hot_fn]` functions in the crate root file, and keep the
  `use` list at the top of that file stable: the patch machinery re-reads
  it, and a removed import is a patch that cannot compile.
- Whole-crate reloads need nothing special; keep `register` idempotent
  (create on first load, count up on reload), exactly like the worked
  example's marker resource.

## 9. Checking your work

- The editor logs every registration and execution with stable tokens
  (`[editor] <crate> loaded`, `[editor] panel registered`,
  `[editor] module panel built`, `[editor] command executed`,
  `[editor] history push`, ...). One line per interesting transition is the
  house style; tests read them.
- The scripted test channel drives a running editor from a file of
  commands: `command <id>`, `panel <id>`, `activate <widget name>`,
  `click x y`, `key <key> <target>`, `tab <title>`, `select <handle>`,
  `edit ...`, `dump ...`, `window-place <id> x y`, `quit`. The suites in
  `devops/tests/test_editor_*.py` are the reference for scripting a run.
- Add a `// NOTE: asserted on by ...` comment above a log line a suite
  depends on; that convention keeps intent visible when the line moves.
- An end-to-end suite of your own joins the editor group by carrying
  `SETS: editor` in its docstring: `run_all.py --set editor` runs the whole
  set, and the gate's `editor` step (`verify.py --only editor`) runs it
  right after building `pill_editor_native`, the order the suites need.

## 10. Checklist for a new editor part

1. `modules/extensions/<extension>/<extension>_editor/` exists, with the
   `Cargo.toml` and root file shape of section 1.
2. `register` is `#[pill_module]`, returns `0`, and is idempotent across
   reloads.
3. Panels are registered as one set per crate; commands carry the crate's
   owner name and a namespaced id.
4. Every owned command id is drained and retired in the crate's own system.
5. Undoable edits go through `push_recorded`; nothing else writes the field.
6. Windows and dialogs name registered panels; ids are stable strings.
7. State that must survive a reload is a persistable resource with a
   `shared_name`.
8. Panel bodies and handlers carry `#[pill_hot_fn]`; the root file's `use`
   list is stable.
9. The editor loads the crate (log line), the panel opens (`panel <id>`),
   an edit runs and undoes (`command <id>`, `command editor.undo`).

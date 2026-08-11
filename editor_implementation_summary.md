# Editor Implementation Summary

## Current status

The `editor` crate is a working Dioxus Desktop frontend around the same `host`
and ECS engine used by `standalone`. It currently provides:

- A native desktop editor window.
- One continuously running ECS game and renderer.
- A dockable, nested tab layout.
- Tab selection, movement, reordering, closing, reopening, and edge docking.
- Horizontal and vertical splitters.
- A renderer viewport aligned with the visible Scene panel.
- Native OS pop-out windows for tabs, including the live Scene renderer.
- Versioned layout persistence and reset-to-default recovery.
- A custom `hello1` / `hello2` context menu instead of the WebView menu.
- A 10 Hz Dioxus FPS display while the engine continues at its uncapped rate.

The editor is usable as an implementation foundation, but it is not yet a
complete production docking framework. The final sections describe the known
limitations and remaining work.

## Running the editor

```powershell
cargo run --package editor
```

The host backend is selected in the same way as for `standalone`. For example:

```powershell
$env:ECS_HOT_RELOAD_MODULE="rs"; cargo run --package editor
```

The editor creates the host, builds and loads the selected game module, starts
the source watcher, and then enters the Dioxus/Tao event loop.

## What to expect

The default workspace contains these panels:

```text
+-------------------+---------------------------------+------------------+
| Hierarchy         | Scene                           | Inspector        |
|                   |                                 +------------------+
|                   |                                 | Statistics       |
+-------------------+---------------------------------+------------------+
| Console                                                              |
+----------------------------------------------------------------------+
```

Hierarchy, Inspector, and Console are currently placeholders. Scene contains
the engine-rendered game and its FPS overlay. Statistics displays FPS and the
number of ECS entities.

The small controls beside each tab are:

- `↗`: undock the tab into a separate OS window.
- `x`: close a closeable tab.

The toolbar in the lower-right corner can reopen closed panels and reset the
entire layout. Closing an undocked window returns its panel to the main dock.

Right-clicking in the main or detached windows opens the custom editor context
menu containing `hello1` and `hello2`. These entries currently only close the
menu; application commands have not been attached to them yet.

## High-level architecture

```text
Dioxus/Tao event loop
        |
        +-- main editor VirtualDom
        |      |
        |      +-- LayoutModel signal
        |      +-- pure geometry snapshot
        |      +-- dock chrome and panel hosts
        |      +-- redraw-driven engine frame pump
        |
        +-- zero or more detached VirtualDoms
               |
               +-- one native window per detached panel
               +-- shared EditorContext
               +-- close notification mailbox

EditorContext
        |
        +-- RenderingHost
               |
               +-- Host / game module / file watcher
               +-- ECS Engine and scheduler
               +-- engine-owned wgpu Renderer
```

There is only one ECS world and one loaded game. Creating a pop-out does not
start a second game instance.

## Important files

| File | Responsibility |
| --- | --- |
| `editor/src/main.rs` | Window creation, Dioxus root, frame pump, renderer bridge, and shared editor context. |
| `editor/src/dock_view.rs` | Dock UI, tab and splitter input, drag/drop behavior, panel factory, context menu, and undock control. |
| `editor/src/popout.rs` | Additional native windows, detached VirtualDoms, close/redock mailbox, and Scene surface transfer. |
| `editor/src/layout/model.rs` | Serializable node arena, panels, rows, tabsets, IDs, and validation. |
| `editor/src/layout/action.rs` | Complete list of supported layout mutations. |
| `editor/src/layout/reducer.rs` | Transactional mutations, normalization, and reducer tests. |
| `editor/src/layout/geometry.rs` | Pure recursive rectangle, splitter, tab, drop-zone, and Scene geometry calculation. |
| `editor/src/layout/persistence.rs` | Loading, validation, recovery, and saving of the layout JSON. |
| `editor/assets/dock_layout.css` | Dock theme, checker background, overlays, tabs, menus, and pop-out styling. |
| `host/src/runtime.rs` | Shared engine frame API and renderer surface retargeting. |
| `engine/src/renderer.rs` | Window surface, device, queue, resize, viewport selection, and presentation. |
| `engine/src/render.rs` | Sprite pipeline, physical viewport/scissor, and virtual-resolution projection. |

## Startup and frame execution

`main()` creates a transparent Tao window through Dioxus Desktop. During the
window creation callback it constructs one `EditorContext`, which owns the
`RenderingHost` and the main window handle.

The frame sequence is:

1. Dioxus requests the first native redraw.
2. `RedrawRequested` calls `RenderingHost::run_one_frame()`.
3. The host processes hot reload, executes the ECS scheduler, runs the native
   compatibility update hook, and updates FPS accounting.
4. The engine renderer draws the current world and presents the wgpu surface.
5. A Dioxus coroutine yields once and requests the next redraw.

This creates an uncapped continuous loop without a dedicated render thread.
The next redraw is requested only after the previous ECS update and present
have completed.

The host produces its console report every three seconds. The editor samples
live statistics every 100 ms, so only the FPS widget is invalidated at roughly
10 Hz. The complete dock UI is not reconciled for every engine frame.

## Dock model

The layout uses a normalized node arena with stable `NodeId` values:

- `Row`: horizontal or vertical weighted children.
- `TabSet`: an ordered tab list and selected tab.
- `Tab`: title, `PanelKind`, and closeability.

Supported panels are Scene, Hierarchy, Inspector, Console, and Statistics.
Scene and the other built-in panel kinds are singletons.

All mutations go through `LayoutAction` and the reducer. Current actions are:

- Select a tab.
- Move or reorder a tab.
- Dock a tab to a center or edge.
- Resize an adjacent split pair.
- Close or reopen a panel.
- Detach a tab into another window.
- Reset the workspace.

The reducer applies every action to a clone, normalizes it, validates the
complete result, and commits only on success. A rejected action leaves the
original model unchanged.

Normalization removes empty tabsets, collapses one-child rows, flattens safe
same-axis rows, normalizes weights, and repairs the active tabset. Detaching
the final remaining tab is rejected because the dock root must remain valid.

## Geometry and drag/drop

`compute_layout()` is a pure function. It receives the model, available
logical rectangle, and shared metrics, and produces a `LayoutSnapshot` with:

- Row and tabset rectangles.
- Tab-strip and tab-button rectangles.
- Selected panel rectangles.
- Splitter rectangles.
- Drop targets and previews.
- The selected Scene content rectangle.

The DOM and native renderer consume this same snapshot, avoiding delayed DOM
measurement and keeping the Scene aligned during splitter and docking changes.

Tab dragging currently uses pointer movement with a five-pixel activation
threshold. Tab-button insertion targets take priority over tabset docking
zones. Center docking joins a tabset; left, right, top, and bottom docking
create the required split structure.

Splitter dragging changes only the adjacent pair of weights. Arrow keys can
also adjust a focused splitter.

## Rendering inside the Scene panel

A DOM element cannot be used directly as a wgpu surface. The editor therefore
uses one native surface for the complete OS window and treats the Scene panel
as a transparent opening above that surface.

The alignment path is:

1. The layout solver calculates the Scene rectangle in logical CSS pixels.
2. `EditorContext` converts its edges to physical pixels using Tao's scale
   factor.
3. `RenderingHost::set_render_viewport()` forwards the rectangle to the engine.
4. wgpu applies the rectangle as both a GPU viewport and a scissor rectangle.
5. Opaque Dioxus panels cover the rest of the native surface.

When Scene is hidden or detached, the main-window Scene rectangle becomes
empty and drawing into the main dock is disabled.

The game uses a stable virtual resolution of 800 x 600. The physical viewport
can have any size; wgpu scales the complete virtual coordinate space to fill
the current Scene rectangle. This currently stretches independently on both
axes, so a viewport with a different aspect ratio can visibly distort sprites.

The swapchain remains the size of the OS window. Only viewport/scissor state
matches the dock panel; no swapchain is recreated for ordinary dock resizing.

## Native tab pop-outs

Clicking `↗` applies `DetachTab` and calls Dioxus Desktop's `new_window()`.
Every detached window is a real top-level OS window with its own WebView and
independent `VirtualDom`, but it runs on the existing Tao event loop.

Detached windows share the main `EditorContext`. A `PopoutManager` provides a
small synchronized mailbox because a detached VirtualDom must not directly
mutate a signal owned by the main VirtualDom.

Closing a pop-out works as follows:

1. Its scoped Tao handler receives `CloseRequested`.
2. Cleanup runs before Dioxus destroys the window.
3. The panel identity is placed in the redock mailbox.
4. The main redraw loop drains the mailbox.
5. The panel is reopened in the active surviving tabset and the layout is
   saved.

Cleanup is guarded by an `AtomicBool`, because both `CloseRequested` and the
VirtualDom destruction fallback can observe shutdown. The panel is restored
only once.

### Detached Scene behavior

Scene requires extra handling because it owns native rendering:

1. The editor creates the detached transparent window.
2. `RenderingHost::retarget_render_window()` constructs a renderer for the new
   window while preserving the existing ECS host and world.
3. The detached window uses its complete client area as the physical viewport
   and retains the 800 x 600 virtual resolution.
4. Its resize events update the renderer surface and full-window viewport.
5. Before it closes, a main-window renderer is constructed and the last known
   dock viewport is restored.

This path has been smoke-tested with real native windows: Scene creation,
surface handoff, frame presentation, close, main-surface recreation, and
redocking complete without terminating the editor.

## Context menu

Dioxus's development WebView context menu is disabled in the window
configuration. A root `oncontextmenu` handler also prevents the browser action
and opens the Dioxus-owned menu at the pointer position.

The menu closes on:

- Selecting `hello1` or `hello2`.
- Clicking elsewhere.
- Pressing Escape.

The same behavior is installed in detached windows.

## Persistence

The model is serialized as versioned JSON. On Windows the file is stored at:

```text
%LOCALAPPDATA%\RustHybridEcs\editor_layout.json
```

Only durable model state is stored: IDs, hierarchy, weights, selection, panel
kinds, and the next ID. Pixel rectangles and pointer state are recomputed and
are never serialized.

Loading validates the complete document. Missing or invalid files fall back
to the built-in default layout with a diagnostic instead of crashing.

Saving writes a temporary file and uses a backup/rename sequence on Windows.
The Reset Layout button reconstructs and saves the built-in default.

## Tests and validation

The editor currently has unit coverage for:

- Default geometry and splitter containment.
- Drop-target priority.
- Model serialization round trips.
- Tab movement and edge docking.
- Adjacent split resizing.
- Close and empty-branch normalization.
- Transactional rejection.
- Scene detachment and final-tab protection.
- Pop-out close-notification draining.
- Logical-to-physical viewport conversion.
- Layout-file round trips.

The current validation baseline is:

- 16 editor tests passing.
- 17 host tests passing with rendering enabled.
- `cargo check --workspace` passing.
- Native smoke validation for ordinary and Scene pop-out windows.

## Practical tips

- Use Reset Layout if a development change makes a saved layout inconvenient.
- Keep Scene selected while diagnosing renderer alignment; an unselected Scene
  intentionally produces an empty main viewport.
- Test DPI work at 100%, 125%, 150%, and 200%. Coordinates cross from logical
  Dioxus units to physical wgpu pixels at one explicit boundary.
- If rendering disappears after GPU or window-lifecycle changes, inspect both
  the selected Scene rectangle and the renderer's current target window.
- Avoid accessing a Dioxus `Signal` from a detached VirtualDom. Send a small
  message to the owning runtime, as `PopoutManager` does.
- Keep expensive engine work out of Dioxus component rendering. ECS execution
  belongs in the native redraw path, while UI signals should update slowly.
- Add new layout mutations through `LayoutAction`; do not edit model nodes
  directly from event handlers.
- Preserve the distinction between virtual game resolution, physical Scene
  viewport, and full native surface size.

## Known issues and limitations

### Pop-out lifecycle

- Closing a pop-out redocks it into the current active tabset, not necessarily
  its exact original tabset and split position.
- Pop-out window positions, sizes, monitor assignment, and maximized state are
  not persisted.
- A detached panel has a new VirtualDom, so arbitrary component-local Dioxus
  state is remounted rather than transferred. Durable panel state should move
  into a shared editor model before panels become complex.
- Renderer retargeting constructs a new wgpu renderer, adapter/device, and
  surface. It is correct but can cause a visible hitch; reusing the device and
  replacing only the surface would be more efficient.
- The main editor window owns the continuous redraw pump. If the main window
  is closed while pop-outs remain, the detached UI may survive but the engine
  frame pump is no longer guaranteed to continue.
- Detachment itself is not immediately persisted. This protects the saved
  dock from a temporary pop-out, but another saved layout action while a panel
  is detached can serialize a model without that panel.
- Popup creation failure is not yet transactional with the preceding
  `DetachTab` action.

### Docking and input

- Root-edge docking is not implemented; edge targets are relative to existing
  tabsets.
- Pointer capture is not implemented. Leaving the WebView currently cancels a
  drag and can make aggressive cross-window pointer movement feel abrupt.
- There is no drag ghost or complete set of simultaneous target indicators.
- Touch and pen cancellation paths are not verified.
- Tab-strip overflow scrolling and overflow menus are missing.
- Minimum pane sizes are only partially enforced during pointer dragging and
  are not fully solved by the geometry pass.
- Double-click splitter equalization is missing.

### Rendering

- The 800 x 600 virtual view stretches to fill Scene. Aspect-preserving
  letterboxing or configurable scaling modes are not implemented.
- Explicit Tao `ScaleFactorChanged` handling is still missing; the current
  bridge generally relies on the accompanying resize event.
- The renderer clears the complete native surface even though sprites are
  clipped to Scene. This should be profiled for larger windows and more complex
  render graphs.
- Only one live Scene renderer is supported because there is one ECS world and
  one renderer target at a time.

### UI and accessibility

- Hierarchy, Inspector, and Console contain placeholder content.
- The context-menu entries are demonstrations without commands.
- Close and undock controls are interactive spans nested in a tab button;
  proper sibling buttons and complete keyboard semantics are still needed.
- Splitters do not expose complete ARIA value metadata.
- Focus restoration after close, move, dock, reset, and pop-out operations is
  incomplete.
- Keyboard tab movement and traversal between tabsets are missing.

### Architecture and persistence

- `main.rs` still contains application, frame-loop, and renderer-bridge logic
  that should be split into `app.rs` and `renderer_bridge.rs`.
- Panels should move into dedicated modules with a registry/factory.
- Drag logic should move out of `dock_view.rs` into a controller.
- CSS and Rust layout metrics are duplicated and can drift.
- Layout writes are immediate rather than debounced.
- The Windows backup/rename persistence path has a short interval without the
  primary file and is not a true atomic replacement.
- There is no migration infrastructure beyond rejecting unknown schema
  versions.

## Recommended next work

### Priority 1: harden pop-outs

1. Add an explicit pop-out registry with stable window IDs and original dock
   placement metadata.
2. Restore each panel to its original tabset/index or reconstruct its original
   split when the source tabset was normalized away.
3. Persist pop-out bounds and monitor information.
4. Move the engine frame pump to an application-level native scheduler so it
   survives main-window closure and minimization.
5. Reuse the existing wgpu device/queue when changing surfaces.
6. Make window creation transactional and automatically undo detachment on
   failure.

### Priority 2: complete docking behavior

1. Implement root-edge docking.
2. Add pointer capture, cancellation, drag ghosts, and all-target overlays.
3. Enforce minimum sizes in the geometry solver.
4. Add tab overflow behavior and touch/pen coverage.
5. Preserve panel state across main/detached VirtualDoms through shared panel
   models.

### Priority 3: improve renderer presentation

1. Add stretch, fit, and integer-scale viewport modes.
2. Add letterbox/pillarbox color configuration.
3. Handle scale-factor transitions explicitly.
4. Profile full-surface clearing and surface recreation.

### Priority 4: editor content and polish

1. Replace placeholder panels with real hierarchy, inspector, and console
   implementations.
2. Introduce a panel registry rather than matching `PanelKind` in the view.
3. Connect context-menu items to editor commands.
4. Complete keyboard navigation, focus restoration, and ARIA semantics.
5. Add Dioxus interaction tests and multi-window lifecycle tests to CI.

For the original detailed docking design and longer acceptance checklist, see
`layout_implementation.md`.

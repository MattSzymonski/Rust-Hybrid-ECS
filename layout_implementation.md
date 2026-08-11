# Dockable Editor Layout Implementation Plan

## Objective

Replace the editor's fixed grid with a FlexLayout-style dockable workspace
implemented natively in Rust and Dioxus. The first complete version will
support:

- Nested horizontal and vertical tabsets.
- Selecting and reordering tabs.
- Moving tabs between tabsets.
- Docking tabs to the center or any edge of a tabset.
- Resizing adjacent panes with splitters.
- Closing and reopening permitted tabs.
- Saving and restoring the workspace.
- Preserving panel component state when tabs move.
- Keeping the native wgpu viewport aligned with a movable Scene tab.

The engine loop remains independent of Dioxus reconciliation. Layout state
changes only during UI interactions, while the existing statistics widget
remains the only component updated periodically.

## Current implementation status

The core dock model and an interactive editor version are implemented:

- [x] Serializable, validated node model with atomic reducer actions.
- [x] Nested weighted rows, tabsets, tab selection, close, reopen, and reset.
- [x] Stable keyed panel hosts that retain Dioxus component state.
- [x] Basic horizontal and vertical splitter resizing with keyboard controls.
- [x] Tab reordering and movement between tabsets.
- [x] Center and four-edge docking relative to tabsets, with live previews.
- [x] Geometry-driven Scene viewport alignment and clipping.
- [x] Versioned layout persistence with invalid-file recovery.
- [x] Initial tab/separator ARIA roles and arrow-key navigation.
- [x] Model, reducer, geometry, and persistence unit tests.
- [x] Workspace, host, renderer, and responsive-window smoke validation.

The first complete version is not finished yet. The checklist below records the
remaining work independently from the advanced features deliberately deferred
at the end of this document.

## Remaining work for the first complete version

### Docking and pointer behavior

- [ ] Add root-edge docking so a tab can split the complete workspace, not only
  an existing tabset.
- [ ] Capture the active pointer during tab and splitter drags. Dragging outside
  the layout or WebView must end or cancel cleanly without leaving stale state.
- [ ] Add an explicit drag ghost and show all eligible drop indicators, while
  retaining the current active-target preview.
- [ ] Handle touch and pen input through the same pointer state machine and
  verify pointer cancellation paths.
- [ ] Add tab-strip overflow scrolling and then an accessible overflow menu.

### Geometry and resizing

- [ ] Enforce minimum pane width and height in the geometry solver itself. The
  current pointer clamp is only a partial guard and does not constrain initial,
  restored, or very small layouts.
- [ ] Redistribute constrained row space deterministically when several
  children reach their minimum size.
- [ ] Add double-click splitter equalization.
- [ ] Remove duplicated Rust/CSS metric values by generating them from one
  source or add a parity test that fails when they diverge.
- [ ] Handle Tao scale-factor changes explicitly instead of relying on an
  accompanying resize event.

### Architecture cleanup

- [ ] Finish extracting `main.rs` into `app.rs` and `renderer_bridge.rs`.
- [ ] Move editor panels into the planned `panels/` modules.
- [ ] Replace the direct `PanelKind` match with a panel registry/factory so new
  editor panels do not require changes in the dock view.
- [ ] Move drag logic out of `dock_view.rs` into a dedicated controller and
  isolate drag-overlay invalidation from tabset/panel reconciliation.

### Persistence

- [ ] Debounce layout writes rather than saving every durable selection or tab
  action immediately.
- [ ] Use a truly atomic replace operation on Windows. The current backup and
  rename fallback is recoverable but has a short interval without the primary
  file.
- [ ] Add schema-migration infrastructure before introducing a second persisted
  layout version.
- [ ] Preserve unknown optional panel configuration where possible.

### Accessibility and keyboard controls

- [ ] Expose splitter `aria-valuemin`, `aria-valuemax`, and `aria-valuenow`.
- [ ] Make close controls proper keyboard-operable buttons without nesting
  invalid interactive HTML.
- [ ] Verify focus restoration after close, move, dock, reset, and overflow-menu
  operations.
- [ ] Add keyboard commands for moving tabs and traversing tabsets.
- [ ] Add an accessibility test pass for tab, tablist, tabpanel, separator, and
  menu semantics.

### Automated validation and profiling

- [ ] Add Dioxus interaction tests for selection, reorder, cross-tabset moves,
  splitter dragging, edge docking, cancellation, close, reopen, and reset.
- [ ] Add a stateful test panel proving component state survives selection and
  cross-tabset movement.
- [ ] Test that hiding Scene sends an empty native viewport and selecting it
  restores the correct physical rectangle without recreating the surface.
- [ ] Test resize and display-scale transitions at 100%, 125%, 150%, and 200%.
- [ ] Add corrupted-file and interrupted-write persistence tests.
- [ ] Profile drag-time Dioxus reconciliation and confirm idle engine FPS is not
  affected by the dock model.

The features in **Explicitly deferred features** remain future work after this
checklist is complete.

## Reference architecture

The design is inspired by `Z:\OtherProjects\Other\FlexLayout`, especially its
separation of model, actions, geometry, rendering, and drag/drop orchestration.
React-specific implementation details will not be copied.

| FlexLayout concept | Rust/Dioxus equivalent |
| --- | --- |
| `Model`, `RowNode`, `TabSetNode`, `TabNode` | Serializable `LayoutModel` with stable node IDs |
| `Actions` and `Model.doAction()` | `LayoutAction` and one validated reducer |
| `ModelLayout` | Pure `compute_layout()` geometry pass |
| `LayoutInternal`, `Row`, `TabSet`, `Splitter` | Dioxus dock chrome driven by a `LayoutSnapshot` |
| `DragDropManager`, `DropInfo`, `DockLocation` | Pointer-driven `DragController` and `DropTarget` |
| `TabContentRenderer` | Stable keyed `PanelHostLayer` |
| JSON import/export | Versioned Serde document |
| Theme styles | Editor CSS with shared custom properties |

The reference is MIT licensed, but this should be an original, idiomatic Rust
implementation rather than a line-by-line TypeScript port.

## Critical rendering constraint

Dioxus Desktop places a WebView above one native wgpu surface. A DOM element
cannot itself become a native wgpu surface. The Scene tab must therefore be a
transparent window into the existing surface:

1. The layout solver calculates the selected Scene panel's content rectangle
   in logical window coordinates.
2. Dioxus positions the transparent Scene panel from that rectangle.
3. The same rectangle is converted to physical pixels with the Tao scale
   factor.
4. `RenderingHost::set_render_viewport()` receives the resulting rectangle.
5. wgpu applies both viewport and scissor clipping.
6. When Scene is hidden or unselected, an empty viewport disables drawing.

The Rust geometry pass, not delayed DOM measurement, will be the source of
truth. This keeps HTML and wgpu pixel-aligned during docking and splitter
drags, including on high-DPI displays.

## Proposed editor structure

```text
editor/
|- assets/
|  `- dock_layout.css
`- src/
   |- main.rs                 Window creation only
   |- app.rs                  Dioxus root and frame/event integration
   |- renderer_bridge.rs      Scene rectangle -> RenderViewport
   |- panels/
   |  |- mod.rs               Panel registry/factory
   |  |- scene.rs
   |  |- hierarchy.rs
   |  |- inspector.rs
   |  |- console.rs
   |  `- statistics.rs
   `- layout/
      |- mod.rs
      |- id.rs
      |- model.rs
      |- action.rs
      |- reducer.rs
      |- geometry.rs
      |- drag.rs
      |- persistence.rs
      `- view.rs
```

The layout core must not depend on wgpu, the ECS, or `RenderingHost`. Its model,
reducer, and geometry tests must run without opening a native window.

## Data model

Use stable IDs in a normalized node arena instead of deeply nested mutable
enums. This makes cross-tree movement explicit and avoids difficult mutable
borrowing.

```rust
struct LayoutModel {
    schema_version: u32,
    root: NodeId,
    nodes: HashMap<NodeId, LayoutNode>,
    active_tabset: Option<NodeId>,
}

enum LayoutNode {
    Row(RowNode),
    TabSet(TabSetNode),
    Tab(TabNode),
}

struct RowNode {
    axis: Axis,
    children: Vec<WeightedChild>,
}

struct WeightedChild {
    node: NodeId,
    weight: f32,
}

struct TabSetNode {
    tabs: Vec<NodeId>,
    selected: Option<NodeId>,
}

struct TabNode {
    title: String,
    panel: PanelKind,
    closeable: bool,
}
```

`PanelKind` is a serializable editor identifier such as `Scene`, `Hierarchy`,
`Inspector`, `Console`, or `Statistics`. Scene is a singleton because the
current renderer owns one surface and one active viewport.

Every mutation validates and normalizes these invariants:

- IDs are unique and every referenced ID exists.
- Every node has exactly one structural parent.
- Rows contain rows or tabsets; tabsets contain only tabs.
- Weights are finite, positive, and normalized.
- A selected tab belongs to its tabset.
- Empty tabsets are removed.
- Single-child rows are collapsed.
- Adjacent rows with the same axis are flattened when safe.
- The root always resolves to a row or tabset.
- The singleton Scene panel cannot be duplicated.

Invalid persisted models should log a useful diagnostic and fall back to the
default layout rather than panic during rendering.

## Actions and reducer

All changes go through one reducer:

```rust
fn reduce(
    model: &mut LayoutModel,
    action: LayoutAction,
) -> Result<LayoutChange, LayoutError>;
```

Initial actions:

- `SelectTab { tab }`
- `MoveTab { tab, target_tabset, insertion_index }`
- `DockTab { tab, target_tabset, location, ratio }`
- `ResizeSplit { row, splitter_index, first_weight, second_weight }`
- `CloseTab { tab }`
- `OpenTab { panel, target }`
- `SetActiveTabSet { tabset }`
- `ResetLayout`

`DockLocation` contains `Center`, `Left`, `Right`, `Top`, and `Bottom`. Center
inserts into an existing tabset. An edge creates a sibling tabset, reusing the
parent row when its axis matches or inserting a perpendicular nested row.

Actions must be atomic. A rejected action leaves the model unchanged. For
multi-step operations, mutate a clone, validate and normalize it, then commit.
Editor layouts are small enough that this correctness-first transaction is
preferable to partially mutated recovery logic.

## Geometry pass

`compute_layout(model, available_rect, metrics)` produces an immutable
`LayoutSnapshot` containing:

- Rectangles for rows and tabsets.
- Tab-strip, tab-button, and selected-content rectangles.
- Splitter rectangles and their owning row/index.
- Minimum-size constraints.
- Precomputed drop zones.
- The visible Scene content rectangle, if any.

Rows divide available space by child weight after reserving splitter space.
The solver honors minimum pane sizes and redistributes constrained space
deterministically. Splitter dragging modifies only the two adjacent weights.

Geometry uses logical pixels. Conversion to physical pixels happens once in
`renderer_bridge.rs`. Tests should compare exact logical rectangles, with
rounding allowed only at the physical-pixel boundary.

Initial shared metrics:

```text
tab strip height:       30 px
splitter thickness:      5 px
minimum pane width:    120 px
minimum pane height:    80 px
drop edge fraction:     25%
minimum edge drop zone: 32 px
```

Expose matching values as CSS custom properties so rendering and hit testing
cannot silently diverge.

## Dioxus rendering strategy

Render three independent layers in one fixed, transparent root:

1. `DockChromeLayer`: tab strips, opaque backgrounds, borders, and splitters.
2. `PanelHostLayer`: one stable keyed host per tab, positioned from the current
   snapshot.
3. `DragOverlayLayer`: drag preview, valid targets, and active target.

Panel hosts are keyed by `TabId` and rendered from one stable list. Moving a tab
changes only its rectangle and membership; it does not reparent the component
under a different tabset. This mirrors FlexLayout's tab-content renderer and
preserves local state for inspectors, consoles, and future document editors.

Only one panel per tabset is visible. Hidden hosts remain mounted so state
survives selection and movement. A later resource policy may suspend expensive
hidden panels without destroying their state.

The Scene host is transparent. Other panels and all dock chrome are fully
opaque and use the fine gray checker theme. No opaque ancestor may cover the
Scene rectangle.

## Pointer interactions

Do not use browser HTML5 drag-and-drop. Follow FlexLayout's custom drag manager
approach with pointer events so mouse, pen, and touch share one state machine.

Tab drag lifecycle:

1. Pointer down records the tab, origin, and source tabset.
2. Movement beyond a threshold starts dragging and captures the pointer.
3. Pointer movement queries drop zones from the current snapshot.
4. The overlay renders the proposed resulting rectangle.
5. Pointer up dispatches one `MoveTab` or `DockTab` action.
6. Escape or pointer cancellation discards the transient drag state.

Drop priority is deterministic:

1. Tab-button insertion slots.
2. Target tabset center.
3. Target tabset edges.
4. Root-layout edges.

Splitter dragging stores the original adjacent weights, computes a clamped
delta, and updates transient weights during movement. Final weights are
committed and persisted on pointer up. Double-click equalization can follow
after the fundamental behavior is stable.

If Scene moves or resizes, each snapshot immediately updates the host viewport.
This is a cheap rectangle change and never recreates the GPU surface.

## Initial workspace

The default model should exercise nested rows and tabsets:

```text
+-------------------+---------------------------------+------------------+
| Hierarchy         | Scene                           | Inspector        |
|                   |                                 +------------------+
|                   |                                 | Statistics       |
+-------------------+---------------------------------+------------------+
| Console                                                              |
+----------------------------------------------------------------------+
```

Hierarchy, Inspector, and Console may initially be lightweight placeholders.
Scene hosts native rendering, and Statistics reuses the isolated 10 Hz widget.

## Renderer bridge behavior

The current fixed viewport constants must be removed. Instead:

- Tao resize and scale-factor events update the logical root size.
- A memoized layout snapshot exposes the selected Scene content rectangle.
- The bridge translates logical `x`, `y`, `width`, and `height` with the current
  scale factor and clamps them to the physical surface.
- Hidden Scene produces `RenderViewport::new(0, 0, 0, 0)` rather than `None`,
  because `None` currently means full-surface rendering.
- Identical rectangles are deduplicated before borrowing `RenderingHost`.
- Splitter and drag updates are applied immediately; no WebView JavaScript
  measurement round trip is required.

The bridge receives snapshots through a narrow API. Layout code must never
borrow or mutate the ECS host directly.

## Persistence

Serialize a versioned Serde document. Persist IDs, hierarchy, weights,
selection, panel kinds, and serializable panel configuration. Never serialize
pixel rectangles, pointer state, or computed drop zones.

Rules:

- Save after completed interactions, with a short debounce.
- Write to a temporary file and atomically rename it.
- Keep a built-in default layout or checked-in JSON asset.
- Reject unknown schema versions with a clear diagnostic.
- Fall back safely when validation fails.
- Provide a visible `Reset Layout` command.
- Inject the persistence path in tests; never touch the real user profile.

The concrete user-data directory can be selected during implementation with a
small platform-directory crate.

## Accessibility

Implement baseline tab accessibility from the start:

- Tab strips use `role="tablist"`.
- Buttons use `role="tab"`, `aria-selected`, and visible focus.
- Content uses `role="tabpanel"` and references its tab label.
- Arrow keys move focus within a strip.
- Enter or Space selects the focused tab.
- Splitters use `role="separator"`, expose orientation, and support arrow-key
  resizing.
- Escape cancels an active drag.

Keyboard tab movement and global tabset traversal can follow mouse docking, but
the reducer actions must never assume pointer input.

## Implementation stages

### Stage 1: Extract and protect the current editor

- Split launch, app loop, statistics, and viewport bridging out of `main.rs`.
- Preserve uncapped engine rendering and 10 Hz statistics updates.
- Keep the current fixed workspace working while the model is built.

Exit condition: behavior is unchanged, the workspace builds, and current
engine/host/editor tests remain green.

### Stage 2: Model, actions, and serialization

- Implement IDs, node types, defaults, validation, and normalization.
- Implement select, move, center-dock, edge-dock, close, open, and reset.
- Add versioned Serde round trips.
- Build reducer tests before connecting Dioxus interaction.

Exit condition: complex layouts can be created and transformed entirely in
headless unit tests.

### Stage 3: Geometry and static Dioxus layout

- Implement recursive weighted layout and splitter rectangles.
- Render tabsets, tab strips, selected contents, and splitters from snapshots.
- Add the stable panel host layer and panel factory.
- Replace the fixed CSS grid with the default dock model.
- Feed the computed Scene rectangle to the renderer bridge.

Exit condition: the default workspace renders correctly, changing the model in
code moves Scene, and drag behavior is not yet required.

### Stage 4: Splitter interaction

- Add pointer capture and horizontal/vertical resizing.
- Enforce minimum sizes and adjacent-only weight updates.
- Update Scene continuously during resizing.
- Add keyboard resizing and persist on completion.

Exit condition: splits resize without gaps, overlap, or viewport drift at 100%,
125%, 150%, and 200% display scaling.

### Stage 5: Tab selection and movement

- Add selection, close buttons, reordering, and moves between tabsets.
- Verify panel state survives every move.
- Add tab-strip scrolling before an overflow popup.

Exit condition: tabs move between existing tabsets without remounting their
panel components.

### Stage 6: Edge docking and overlays

- Add target discovery and preview rectangles.
- Implement tabset-edge and root-edge docking.
- Normalize redundant rows after every drop.
- Cover cancellation and invalid targets.

Exit condition: every edge creates the intended split and the preview exactly
matches the committed layout.

### Stage 7: Persistence and polish

- Load on startup and debounce-save committed changes.
- Add Reset Layout and recovery diagnostics.
- Complete keyboard behavior and ARIA attributes.
- Add overflow menu, context-menu hooks, and theme variables.
- Profile drag-time Dioxus work and idle engine FPS.

Exit condition: customized layouts survive restart, corrupt layouts recover,
and all acceptance tests pass.

## Test plan

### Model and reducer

- JSON round-trip preserves IDs, weights, selection, and panel kinds.
- Duplicate IDs, missing children, cycles, and invalid weights are rejected.
- Moving a tab updates source and target exactly once.
- Closing the final tab removes and normalizes the empty tabset.
- Docking in every direction produces the expected axis and child order.
- Repeated moves never leave empty tabsets or single-child rows.
- Failed actions leave the original model unchanged.

### Geometry

- Weighted horizontal and vertical splits produce exact rectangles.
- Splitter thickness is reserved exactly once.
- Minimum sizes clamp extreme movement.
- Nested rows contain no gaps or overlaps.
- Drop zones stay inside targets and have deterministic priority.
- Logical-to-physical Scene conversion works at common scale factors.
- Tiny windows never underflow coordinates.

### Dioxus and renderer integration

- Exactly one panel is visible per tabset.
- A stateful test panel retains state after reorder and cross-tabset movement.
- Statistics updates do not invalidate the dock layout.
- Hidden Scene produces an empty native viewport; selecting it restores it.
- Dock and splitter changes update the viewport without recreating the surface.
- Resize and scale-factor events recompute geometry and native clipping.

### Manual smoke matrix

- Drag rapidly over all target types and cancel with Escape.
- Resize the native window during and after interactions.
- Test Windows display scaling at 100%, 125%, 150%, and 200%.
- Hot-reload Rust and C# games with Scene docked in different positions.
- Run for an extended period and check for pointer-listener, coroutine, panel,
  or WebView leaks.

## Acceptance criteria

- The default workspace contains multiple tabsets and resizable splits.
- Tabs reorder, move between tabsets, and dock to all four edges.
- Drop previews match final geometry.
- Panel-local state survives selection and movement.
- Scene moves and resizes while wgpu remains pixel-aligned and clipped.
- Hidden Scene cannot draw beneath another panel.
- Engine and renderer retain their own cadence; Dioxus does not reconcile the
  layout on each engine frame.
- Layouts save, restore, validate, and recover to defaults when corrupt.
- Model and geometry behavior has deterministic unit coverage.
- `cargo check --workspace` and relevant editor, host, and engine tests pass.

## Explicitly deferred features

These FlexLayout features should not delay the first complete docking workflow:

- Native popout windows and multi-monitor restoration.
- Floating panels.
- Border tabsets and overlay borders.
- Nested independent submodels.
- Maximized tabsets.
- Pinned tabs and inline renaming.
- Undo/redo layout history.
- Animated docking transitions.

The model and action API should leave room for these features without adding
premature implementation complexity.

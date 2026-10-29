"""End-to-end acceptance for the egui editor frontend.

PROTECTS: The egui editor: startup, shell module, dock rearrangement, patch and reload state survival.
TIER: editor
SETS: editor

Builds against an already-built `pill_editor` binary - verified before use, so
a binary older than the sources it was compiled from is refused rather than
trusted - and starts it on
`italian_brainrot` - a three-entity project, so the frames are fast and the
heartbeat waits are short - with the scripted input channel
(`PILL_EDITOR_TEST_COMMANDS`), and asserts on its log tokens: startup and the
two scene surfaces (the Scene and Scene 2 viewports), the egui shell module
loading, both tabs reporting rows, a scene click that picks an entity through
the renderer's pick contract, moves the shared selection and publishes the
outline, the inspector reporting the picked entity's detail and one of that
entity's rows edited through the scripted `edit array` (the history step and
the undo that puts the value back), a corner click
that clears both, scripted gizmo drags that translate, rotate and scale the
selected entity (checked through `dump field`), the Assets pipeline (the
listing, a create through its dialog, a rename that the selection follows, a
move, a delete that undoes back, and the write-meta action on a staged
sourced file), the Inspector's asset-settings rows round-tripping through
`edit asset-settings` (a boolean row and an enum row, each applied to the
asset's `.meta` sidecar and undone back to its original value), the Systems
tab's data (a scripted `dump systems`, a recorded system toggle with its
undo, and the pause and step commands reaching the engine), the context
menus (a real right-click opening the viewport's menu, and the menu
actions' command paths: the entity create/delete pair with the delete's
undo, a component detach and attach round-trip, and the gizmo tool, snap
and space switches), the Preferences tab (a seeded preferences file whose
step cap the live reader applies at startup, a scripted row whose save
reaches the file and the reader, the saved snapping step steering a
snapped drag, the history trimming at a tiny cap, and a tool choice
persisting into the project's remembered entry), the editor's theme layers
(a user file named by the `theme` preference, a project file under
`res/editor/` that `editor_settings.yaml` re-points, an out-of-scope path
refused with the rule, a legacy name reported and a broken user file
refused while the other layer applies - each checked through `dump theme`),
the Game tab's viewport
(the primary surface's child window placed at the tab's body while that tab
is the visible one, and hidden with it), the floating toolbox (its rows
switching the tool without picking the scene, and a scripted pointer drag
parking it inside the viewport while it keeps taking input), the infinite
scene grid (on over the scene viewports by default, switched off and on
through the toolbox's grid row while the game viewport never draws it), a dock
rearrangement (an undocked
tab) through the scripted channel, a live patch of the shell that keeps its
frame counter running, a structural edit that reloads the shell and keeps the
rearranged dock, a project reload that keeps the editor UI, the second
viewport's undock with its own scene surface going live at the window's
rectangle, and a clean exit.

Each run keeps its log, sandbox and command file under
`local/editor-validation/egui-<run id>/`; the sandbox is imported by the
egui editor's layout persistence (`RustHybridEcs/editor_egui_layout.json`) so
a second run of the suite starts from the first run's layout. The only
sources this suite edits are the shell module's, the example project's
(its `scene.rs` and the two `.meta` sidecars whose settings rows the
Inspector step round-trips through the real apply), all restored when the
session closes; everything else it starts is its own process tree.
"""
import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_KEYBINDING_CONFLICT_TOKEN,
    EDITOR_KEYBINDINGS_APPLIED_TOKEN,
    EDITOR_SHORTCUT_RAN_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
    ScriptedCommandRejected,
    run_directory,
)
from core.paths import MODULES_ROOT, REPOSITORY_ROOT, executable_name  # noqa: E402
from core.suite_common import verify_editor_binary  # noqa: E402
from core import win_clipboard  # noqa: E402

# The suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources. Patterns that
# embed runtime values (frame counts, dock surfaces) compose them with their
# regex suffix at the wait.
EGUI_EDITOR_BINARY = executable_name("pill_editor")
EGUI_SHELL_LOADED_TOKEN = "[editor] pill_editor_shell_egui_editor loaded"
# Every shell generation clears the shared egui context's temporary data
# before its first draw, so nothing a previous generation created is read
# through an unloaded image.
SHELL_CONTEXT_CLAIMED_TOKEN = "[editor] shell context claimed"
EGUI_ROWS_TOKEN = "[editor] egui shell rows"
EGUI_DOCK_DUMP_TOKEN = "[editor] egui dock dump"
EGUI_TAB_UNDOCKED_TOKEN = "[editor] tab undocked"
SCENE_SURFACE_TOKEN = "[editor] scene surface attached"
SCENE_VIEW_ADDED_TOKEN = "[editor] scene view added"
SCENE_VIEW_OPENED_TOKEN = "[editor] scene view opened"
SCENE_SURFACE_RESIZED_TOKEN = "[editor] scene surface resized"
FRAME_HEARTBEAT_TOKEN = "[editor] frame heartbeat"
FRAME_IDLE_TOKEN = "[editor] frame idle"
CURSOR_ICON_TOKEN = "[editor] cursor icon"
SCENE_BADGE_RECT_TOKEN = "[editor] scene badge rect"
SCENE_BADGE_TOKEN = "[editor] scene badge"
PREVIEW_REQUESTED_TOKEN = "[editor] preview requested"
PREVIEW_READY_TOKEN = "[editor] preview ready"
PROJECT_RELOAD_TOKEN = "hot reload complete"
EXTENSION_RELOAD_TOKEN = "extension reload processed"
SCENE_VIEW_RECT_TOKEN = "[editor] scene view rect"
SCENE_PICK_ENTITY_TOKEN = "[editor] scene pick entity"
SCENE_PICK_NOTHING_TOKEN = "[editor] scene pick nothing"
SELECTION_CHANGED_TOKEN = "[editor] selection changed"
OUTLINE_SET_TOKEN = "[editor] outline set"
OUTLINE_CLEARED_TOKEN = "[editor] outline cleared"
SCENE_PICK_HANDLE_TOKEN = "[editor] scene pick handle"
GIZMO_ANCHOR_TOKEN = "[editor] gizmo anchor"
GIZMO_TOOL_TOKEN = "[editor] gizmo tool"
GIZMO_DRAG_PRESS_TOKEN = "[editor] gizmo drag press"
GIZMO_DRAG_STARTED_TOKEN = "[editor] gizmo drag started"
GIZMO_DRAG_ENDED_TOKEN = "[editor] gizmo drag ended"
GIZMO_DRAG_RELEASED_TOKEN = "[editor] gizmo drag ended without a move"
GIZMO_HOVER_TOKEN = "[editor] gizmo hover handle"
GIZMO_HOVER_NONE_TOKEN = "[editor] gizmo hover none"
GIZMO_GUIDES_TOKEN = "[editor] gizmo guides"
VIEWPORT_DUMP_TOKEN = "[test] viewport"
VIEWPORT_CAMERA_TOKEN = "[test] viewport camera"
MENU_BAR_RECT_TOKEN = "[editor] menu bar rect"
MENU_TITLE_RECT_TOKEN = "[editor] menu item rect"
MENU_ENTRY_RECT_TOKEN = "[editor] menu entry rect"
MENU_OPENED_TOKEN = "[editor] menu opened"
MENU_ACTION_TOKEN = "[editor] menu action"
PALETTE_OPENED_TOKEN = "[editor] palette opened"
PALETTE_QUERY_TOKEN = "[editor] palette query"
PALETTE_RECT_TOKEN = "[editor] palette rect"
PALETTE_CLOSED_TOKEN = "[editor] palette closed"
COMMAND_NO_ACTION_TOKEN = "[editor] command has no action"
PALETTE_DISMISSED_TOKEN = "[editor] palette dismissed"
CONSOLE_DUMP_TOKEN = "[test] console"
PROBLEMS_DUMP_TOKEN = "[test] problems"
PROBLEM_ROW_TOKEN = "[test] problem row"
PROBLEM_OPENED_TOKEN = "[editor] problem opened"
STATS_DUMP_TOKEN = "[test] stats"
STATS_TAB_TOKEN = "[test] stats tab"
CADENCE_DUMP_TOKEN = "[test] cadence"
REDRAW_ON_DEMAND_TOKEN = "[editor] redraw on demand"
NO_OVERLAY_TOKEN = "[editor] no overlay posture"
OVERLAY_POSTURE_TOKEN = "[editor] overlay posture"
PANELS_DUMP_TOKEN = "[test] panels"
PANEL_ROW_TOKEN = "[test] panel"
PANEL_REGISTERED_TOKEN = "[editor] panel registered"
PANEL_OVER_BUDGET_TOKEN = "[editor] module panel over budget"
PANEL_UNLOADED_TOKEN = "[editor] panel source unloaded"
PANEL_RESTORED_TOKEN = "[editor] panel source restored"
PANEL_WINDOW_OPENED_TOKEN = "[editor] panel window opened"
PANEL_WINDOW_REQUESTED_TOKEN = "[editor] panel window requested"
PANEL_MODAL_ANSWERED_TOKEN = "[editor] panel modal answered"
PANEL_LOG_TOKEN = "[editor] panel log"
PANEL_FIELD_QUEUED_TOKEN = "[editor] panel field queued"
SELECTION_DUMP_TOKEN = "[test] selection"
SELECTED_ENTITY_TOKEN = "[test] selected entity"
MIXED_ROW_TOKEN = "[editor] inspector row kind=mixed"
SELECTION_EDIT_TOKEN = "[editor] selection field edit queued"
CLIPBOARD_WRITTEN_TOKEN = "[editor] clipboard written"
CLIPBOARD_PASTED_TOKEN = "[editor] clipboard pasted"
DROP_PREVIEW_TOKEN = "[editor] drop preview"
FILES_DROPPED_TOKEN = "[editor] files dropped"
DROP_REFUSED_FOLDER_TOKEN = "[editor] drop refused: folder"
DROP_REFUSED_PATH_TOKEN = "[editor] drop refused: "
DRAG_OUT_PREPARED_TOKEN = "[editor] drag out prepared"
# No pointer button is held during an automated run, so the drag has to end
# before OLE's loop: that refusal is the deterministic half of drag-out.
DRAG_OUT_NO_BUTTON_TOKEN = "[editor] drag out skipped: no button held"
RECOVERED_STATE_TOKEN = "[editor] recovered editor state after an unclean exit"
# The registered name the showcase component's payload carries (W-0038
# Stage 17): the paste's registry check compares full type names.
SHOWCASE_TYPE_NAME = "italian_brainrot::ShowcaseComponent"
WINDOWS_DUMP_TOKEN = "[test] windows"
WINDOW_ROW_TOKEN = "[test] window"
SAFE_MODE_TOKEN = "[editor] safe mode"
SAFE_MODE_RETRY_TOKEN = "[editor] safe mode retry requested"
CONSOLE_FILTER_TOKEN = "[editor] console filter"
CONSOLE_SEARCH_TOKEN = "[editor] console search"

# One console dump: the filters in force, the counts and the first row the
# filters left (W-0038 Stage 5).
CONSOLE_DUMP_PATTERN = (
    re.escape(CONSOLE_DUMP_TOKEN)
    + r'\s+level="(?P<level>\w+)"\s+channel="(?P<channel>\w+)"'
    + r'\s+search="(?P<search>(?:[^"\\]|\\.)*)"'
    + r"\s+total=(?P<total>\d+)\s+visible=(?P<visible>\d+)"
    + r"\s+engine=(?P<engine>\d+)\s+game=(?P<game>\d+)"
)
QUIT_REQUESTED_TOKEN = "[editor] quit requested"
SCENE_NEW_TOKEN = "[editor] scene new"
SCENE_OPENED_TOKEN = "[editor] scene opened"
SCENE_SAVED_TOKEN = "[editor] scene saved"
SCENE_TITLE_TOKEN = "[editor] scene title"
LAYOUT_SAVED_TOKEN = "[editor] layout saved"
LAYOUT_LEAF_TOKEN = "[editor] egui dock leaf"
INSPECTOR_SECTION_TOKEN = "[editor] inspector section"
INSPECTOR_SECTION_RECT_TOKEN = "[editor] inspector section rect"
INSPECTOR_ADD_RECT_TOKEN = "[editor] inspector add rect"
INSPECTOR_ADD_OPENED_TOKEN = "[editor] inspector add opened"
INSPECTOR_ADD_ENTRY_TOKEN = "[editor] inspector add entry"
INSPECTOR_ROW_TOKEN = "[editor] inspector row"
ACTIVE_VIEWPORT_TOKEN = "[editor] active viewport"
CAMERA_GESTURE_TOKEN = "[editor] camera gesture applied"
CAMERA_ZOOM_TOKEN = "[editor] camera zoom"
FIELD_TOKEN = "[test] field"
ASSET_LISTING_TOKEN = "[editor] asset listing refreshed"
ASSET_SELECTED_TOKEN = "[editor] asset selected"
ASSET_DIALOG_OPENED_TOKEN = "[editor] asset dialog opened"
ASSET_DIALOG_CLOSED_TOKEN = "[editor] asset dialog closed"
ASSET_CREATE_QUEUED_TOKEN = "[editor] asset create queued"
ASSET_RENAME_QUEUED_TOKEN = "[editor] asset rename queued"
ASSET_DELETE_QUEUED_TOKEN = "[editor] asset delete queued"
ASSET_META_QUEUED_TOKEN = "[editor] asset meta queued"
ENGINE_COMMANDS_APPLIED_TOKEN = "[editor] engine commands applied"
HISTORY_PUSH_TOKEN = "[editor] history push"
HISTORY_UNDO_TOKEN = "[editor] history undo"
# An asset reference row and what it names (W-0048): the Inspector logs each
# distinct row once, the applied pick or drop once, and the drop's two ends'
# rectangles once per placement.
REFERENCE_ROW_TOKEN = "[editor] inspector row kind=asset"
REFERENCE_SET_TOKEN = "[editor] asset reference set"
REFERENCE_TILE_RECT_TOKEN = "[editor] assets tile rect"
REFERENCE_ROW_RECT_TOKEN = "[editor] inspector asset row rect"
# An entity reference row and what it names (W-0048 Stage 6): the row logs each
# distinct reference once, the applied edit once, and the scene-pick arming
# once per request.
ENTITY_REFERENCE_ROW_TOKEN = "[editor] inspector row kind=entity"
ENTITY_REFERENCE_SET_TOKEN = "[editor] entity reference set"
ENTITY_REFERENCE_ROW_RECT_TOKEN = "[editor] inspector entity row rect"
ENTITY_REFERENCE_ARMED_TOKEN = "[editor] inspector entity pick armed"
ENTITY_REFERENCE_ANSWERED_TOKEN = "[editor] inspector entity pick answered"
HIERARCHY_ROW_RECT_TOKEN = "[editor] hierarchy row rect"
RESOURCES_DUMP_TOKEN = "[test] resources"
RESOURCE_FIELD_TOKEN = "[test] resource field"
RESOURCE_SELECTED_TOKEN = "[editor] resource selected"
SELECTION_RESTORED_TOKEN = "[editor] selection restored"
ENTITIES_DUMP_TOKEN = "[test] entities"
ENTITY_ROW_TOKEN = "[test] entity"
SHOWCASE_COMPONENT = "ShowcaseComponent"
RESOURCE_EDIT_TOKEN = "[editor] resource edit queued"
GIZMO_RESOURCE_NAME = "pill_editor_core::gizmo::GizmoState"
HISTORY_REDO_TOKEN = "[editor] history redo"
ASSETS_DUMP_TOKEN = "[test] assets"
ASSET_ROW_TOKEN = "[test] asset"
BROWSER_FOLDER_TOKEN = "[editor] browser folder"
BROWSER_DUMP_TOKEN = "[test] browser"
BROWSER_ITEM_TOKEN = "[test] browser item"
# A module's resources are listed and inspected, never written: the section
# says so once per selected asset, and a delete is refused instead of queued.
ASSET_READ_ONLY_ROW_TOKEN = "[editor] inspector row kind=asset-read-only"
ASSET_DELETE_REFUSED_TOKEN = "[editor] delete refused: a module's resources are read-only"
STANDALONE_TYPE_TOKEN = "[test] standalone type"
SYSTEMS_DUMP_TOKEN = "[test] systems"
SYSTEM_ROW_TOKEN = "[test] system"
PAUSE_TOGGLED_TOKEN = "[editor] pause toggled"
STEP_QUEUED_TOKEN = "[editor] step queued"
SYSTEM_EDIT_QUEUED_TOKEN = "[editor] system edit queued"
CONTEXT_MENU_TOKEN = "[editor] context menu"
NEW_ENTITY_TOKEN = "[editor] new entity queued"
DELETE_ENTITY_TOKEN = "[editor] delete entity queued"
SELECTION_FOLLOWED_TOKEN = "[editor] selection followed the move"
SELECTION_CLEARED_TOKEN = "[editor] selection cleared with the delete"
COMMAND_FAILED_TOKEN = "[editor] command failed"
PREFERENCES_DUMP_TOKEN = "[test] preferences"
PREFERENCES_APPLIED_TOKEN = "[editor] preferences applied"
PREFERENCES_SAVED_TOKEN = "[editor] preferences saved"
PREFERENCE_EDIT_QUEUED_TOKEN = "[editor] preference edit queued"
# The editor's theme layers (W-0127 Stage 6): the executable logs each
# resolution, the shell logs each install, the dump reports what the shell
# would install, and a refused or legacy layer says so.
EDITOR_THEME_RESOLVED_TOKEN = "[editor] theme resolved"
EDITOR_THEME_APPLIED_TOKEN = "[editor] theme applied"
EDITOR_THEME_DUMP_TOKEN = "[test] theme"
EDITOR_THEME_REFUSED_TOKEN = "[editor] theme refused layer="
EDITOR_THEME_LEGACY_TOKEN = "[editor] theme name"
# The fonts a theme layer names (W-0127 Stage 7): an install, a refusal.
EDITOR_FONTS_INSTALLED_TOKEN = "[editor] fonts installed"
EDITOR_FONT_REFUSED_TOKEN = "[editor] font refused"
GIZMO_PREFERENCES_WRITTEN_TOKEN = "[editor] gizmo preferences written"
HISTORY_TRIMMED_TOKEN = "[editor] history trimmed"
GAME_VIEW_RECT_TOKEN = "[editor] game view rect"
TAB_ACTIVATED_TOKEN = "[editor] tab activated"
POPOUT_MOVED_TOKEN = "[editor] popout moved"
POPOUT_OPENED_TOKEN = "[editor] popout opened"
POPOUT_CLOSED_TOKEN = "[editor] popout closed"
POPOUT_PRESENTED_TOKEN = "[editor] popout presented"
DOCK_POPOUT_TOKEN = "[editor] egui dock popout"
SCENE_VIEW_REBOUND_TOKEN = "[editor] scene view rebound"
TOOLBOX_RECT_TOKEN = "[editor] toolbox rect"
TOOLBOX_ITEM_TOKEN = "[editor] toolbox item"
TOOLBOX_CLICK_TOKEN = "[editor] toolbox click"
TOOLBOX_MOVED_TOKEN = "[editor] toolbox moved"
POINTER_DRAG_TOKEN = "[editor] pointer drag injected"
SCENE_GRID_TOKEN = "[test] scene grid"
SCENE_GRID_STATE_TOKEN = "[editor] scene grid"
HOT_PATCH_TOKEN = "[hot]"
KEY_INJECTED_TOKEN = "[editor] key injected"
TOAST_TOKEN = "[editor] toast"

# One toast line as the shell logs it, cut after the opening quote of the
# text: the level (a quoted string field), the notice id and the text the
# executable produced, whose own pattern each check appends inside the
# quotes (W-0038 Stage 4).
TOAST_PREAMBLE = re.escape(TOAST_TOKEN) + r'\s+level="{}"\s+seq=\d+\s+text="'

# A whole toast line, rectangle included: the level, the id, the text and
# where the row landed, so a check can click the toast itself.
TOAST_LINE_PATTERN = (
    re.escape(TOAST_TOKEN)
    + r'\s+level="(\w+)"\s+seq=\d+\s+text="((?:[^"\\]|\\.)*)"'
    + r"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)\s+width=([0-9.]+)\s+height=([0-9.]+)"
)

# The transform the gizmo drags edit; the field dump resolves the short
# name, like the native suite's dumps do.
TRANSFORM_COMPONENT = "TransformComponent"

# One live patch of a named shell function: the host's line names the
# function it replaced, the milliseconds it took and the route it took. The
# names are package-qualified, so the pattern only pins the module and the
# function and leaves the crate prefix free.
HOT_PATCH_LIVE_PATTERN = (
    # The shell is split file-per-tab (W-0037 Stage 3), so a body's path
    # carries its module: `...::shell::tab_entities::entities_tab`.
    re.escape(HOT_PATCH_TOKEN) + r" \S*::shell::(?:\w+::)?{} LIVE \d+ ms"
)

# The rows heartbeat's runtime values, in the field order the shell logs them.
ROWS_PATTERN = (
    re.escape(EGUI_ROWS_TOKEN) + r"\s+frames=(\d+)\s+entities=(\d+)\s+systems=(\d+)"
)
DOCK_DUMP_PATTERN = re.escape(EGUI_DOCK_DUMP_TOKEN) + r"\s+surfaces=(\d+)"
# One leaf of the dock dump: the surface and node that hold it, the tab in
# front and every tab it holds, in draw order.
DOCK_LEAF_PATTERN = (
    re.escape(LAYOUT_LEAF_TOKEN)
    + r"\s+surface=(\d+)\s+node=(\d+)\s+active=([a-z0-9]*)\s+tabs=([a-z0-9,]*)"
)
# The scene view rect's runtime values, in the field order the frontend logs
# them: view, x, y, width, height, visible.
SCENE_RECT_PATTERN = (
    re.escape(SCENE_VIEW_RECT_TOKEN)
    + r"\s+view=(\d+)\s+x=(-?\d+)\s+y=(-?\d+)\s+width=(\d+)\s+height=(\d+)"
    + r"\s+visible=(true|false)"
)
# The game view rect's runtime values, in the field order the frontend logs
# them: x, y, width, height, visible. The game window has no view index - one
# primary surface, one window.
GAME_RECT_PATTERN = (
    re.escape(GAME_VIEW_RECT_TOKEN)
    + r"\s+x=(-?\d+)\s+y=(-?\d+)\s+width=(\d+)\s+height=(\d+)"
    + r"\s+visible=(true|false)"
)
# The floating toolbox's runtime values, in the field order the shell logs
# them: the panel's rectangle, and one row's - both in logical points, which
# are the physical pixels of the click injections while the suite runs at a
# scale factor of 1.
TOOLBOX_RECT_PATTERN = (
    re.escape(TOOLBOX_RECT_TOKEN)
    + r"\s+view=(\d+)\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)"
    + r"\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
TOOLBOX_ITEM_PATTERN = (
    re.escape(TOOLBOX_ITEM_TOKEN)
    + r'\s+view=(\d+)\s+item="([a-z]+)"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)'
    + r"\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
# The menu bar's runtime values, in the field order the shell logs them: the
# strip's rectangle, each title's, and each open menu entry's - all in logical
# points, the physical pixels of the click injections while the suite runs at a
# scale factor of 1.
MENU_BAR_RECT_PATTERN = (
    re.escape(MENU_BAR_RECT_TOKEN)
    + r"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
MENU_TITLE_RECT_PATTERN = (
    re.escape(MENU_TITLE_RECT_TOKEN)
    + r'\s+name="([a-z]+)"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)'
    + r"\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
MENU_ENTRY_RECT_PATTERN = (
    re.escape(MENU_ENTRY_RECT_TOKEN)
    + r'\s+menu="([a-z]+)"\s+label="([^"]+)"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)'
    + r"\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
# The Inspector's runtime values: a section header's rectangle (keyed by the
# registered component type), the Add Component button's, and an addable
# entry's - all in logical points like the rest of the shell's geometry.
INSPECTOR_SECTION_RECT_PATTERN = (
    re.escape(INSPECTOR_SECTION_RECT_TOKEN)
    + r'\s+component="([^"]+)"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)'
    + r"\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
INSPECTOR_ADD_RECT_PATTERN = (
    re.escape(INSPECTOR_ADD_RECT_TOKEN)
    + r"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
INSPECTOR_ADD_ENTRY_PATTERN = (
    re.escape(INSPECTOR_ADD_ENTRY_TOKEN)
    + r'\s+component="([^"]+)"\s+x=(-?[0-9.]+)\s+y=(-?[0-9.]+)'
    + r"\s+width=([0-9.]+)\s+height=([0-9.]+)"
)
# One drawn Inspector row, in the field order the shell logs it: the row's
# kind, the component it belongs to, then whatever that kind carries (a
# field's name, widget and tooltip, a header's text, a gap's height). The
# kind logs unquoted because the shell passes it as a literal.
INSPECTOR_ROW_PATTERN = (
    re.escape(INSPECTOR_ROW_TOKEN)
    + r'\s+kind="?([a-z-]+)"?\s+component="([^"]+)"([^\n]*)'
)
# The scene grid's state, in the field order the frontend logs it: the
# switch, the cell size, and the surfaces the renderer draws it on (empty
# while it is off).
SCENE_GRID_PATTERN = (
    re.escape(SCENE_GRID_TOKEN)
    + r"\s+enabled=(true|false)\s+cell=([0-9.]+)\s+surfaces=(\S*)"
)
# The two answers a scripted scene click can produce, as full patterns.
PICK_ENTITY_PATTERN = re.escape(SCENE_PICK_ENTITY_TOKEN) + r"\s+entity=(\S+)"
PICK_NOTHING_PATTERN = re.escape(SCENE_PICK_NOTHING_TOKEN)
# One row of a `dump systems`: index, name, owner, state, then the markers.
SYSTEM_ROW_PATTERN = (
    re.escape(SYSTEM_ROW_TOKEN)
    + r'\s+index=(\d+)\s+name="([^"]+)"\s+owner="([^"]+)"\s+enabled=(true|false)'
    + r"\s+hot_patchable=(?:true|false)\s+ambiguous=(?:true|false)"
)


def wait_for_token(session: EditorSession, pattern: str, timeout: float, label: str) -> str:
    """Waits until `pattern` matches the session's log; returns the match."""
    session.wait(pattern, timeout, label)
    return session.find(pattern).group(0)


def rows_frames(session: EditorSession) -> dict:
    """Every rows heartbeat's `frames` value, mapped to its entity/system counts."""
    seen = {}
    for match in re.finditer(ROWS_PATTERN, session.read_log()):
        seen[int(match.group(1))] = (int(match.group(2)), int(match.group(3)))
    return seen


def occurrence_count(session: EditorSession, pattern: str) -> int:
    """How many times `pattern` appears in the log so far."""
    return len(re.findall(pattern, session.read_log()))


def latest_log_match(session: EditorSession, pattern: str, view: int, label: str) -> "re.Match":
    """The newest log line matching `pattern` whose first group is `view`."""
    matches = [
        match
        for match in re.finditer(pattern, session.read_log())
        if int(match.group(1)) == view
    ]
    if not matches:
        raise AssertionError(f"no {label} for view {view} in the log")
    return matches[-1]


def toolbox_panel(session: EditorSession, view: int) -> tuple:
    """The toolbox panel's last logged rectangle: (x, y, width, height)."""
    match = latest_log_match(session, TOOLBOX_RECT_PATTERN, view, "toolbox rect")
    return (
        float(match.group(2)),
        float(match.group(3)),
        float(match.group(4)),
        float(match.group(5)),
    )


def toolbox_row(session: EditorSession, view: int, item: str) -> tuple:
    """One toolbox row's last logged rectangle, looked up by its name."""
    matches = [
        match
        for match in re.finditer(TOOLBOX_ITEM_PATTERN, session.read_log())
        if int(match.group(1)) == view and match.group(2) == item
    ]
    if not matches:
        raise AssertionError(f"no toolbox row {item!r} for view {view} in the log")
    last = matches[-1]
    return (
        float(last.group(3)),
        float(last.group(4)),
        float(last.group(5)),
        float(last.group(6)),
    )


def toolbox_click(session: EditorSession, view: int, item: str, label: str) -> None:
    """Clicks one toolbox row at its logged center and waits for its token.

    Like a menu item, a toolbox row's click is delivered on one frame and can
    be swallowed while the panel is moving or the pointer is settling, so a
    miss is retried once with the row's rectangle read again.
    """
    pattern = re.escape(TOOLBOX_CLICK_TOKEN) + rf'\s+view={view}\s+item="{item}"'
    for attempt in (1, 2):
        x, y = rect_center(toolbox_row(session, view, item))
        begin = len(session.read_log())
        session.send(f"click {x:.0f} {y:.0f}")
        try:
            session.wait(pattern, 30, label, begin)
            return
        except TimeoutError:
            if attempt == 2:
                raise
            print("  [RETRY] the toolbox row {!r} click missed; retrying once".format(item))


def menu_title_rect(session: EditorSession, name: str) -> tuple:
    """One menu title's last logged rectangle, looked up by its wire name."""
    matches = [
        match
        for match in re.finditer(MENU_TITLE_RECT_PATTERN, session.read_log())
        if match.group(1) == name
    ]
    if not matches:
        raise AssertionError(f"no menu title {name!r} in the log")
    last = matches[-1]
    return (
        float(last.group(2)),
        float(last.group(3)),
        float(last.group(4)),
        float(last.group(5)),
    )


def menu_click(session: EditorSession, menu: str, label: str, action_label: str) -> None:
    """Opens `menu`, clicks the `label` item and waits for its action's log line.

    Every menu is opened fresh: a click on an item closes its menu, so a
    recorded rectangle from an earlier opening would address a closed popup.

    A synthetic click is delivered on one frame, and a frame that is still
    laying the popup out swallows it: the click is retried once, with the
    rectangle read again, before the absence counts as a failure.
    """
    for attempt in (1, 2):
        center = rect_center(menu_title_rect(session, menu))
        session.request(
            f"click {center[0]:.0f} {center[1]:.0f}",
            re.escape(MENU_OPENED_TOKEN) + rf'\s+menu="{menu}"',
            30,
            f"the {menu} menu opened",
        )
        found = [
            match
            for match in re.finditer(MENU_ENTRY_RECT_PATTERN, session.read_log())
            if match.group(1) == menu and match.group(2) == label
        ]
        assert found, f"the {menu} menu did not list {label!r}: " + session.read_log()[-600:]
        entry = found[-1]
        x = float(entry.group(3)) + float(entry.group(5)) / 2.0
        y = float(entry.group(4)) + float(entry.group(6)) / 2.0
        begin = len(session.read_log())
        session.send(f"click {x:.0f} {y:.0f}")
        try:
            session.wait(
                re.escape(MENU_ACTION_TOKEN)
                + rf'\s+menu="{menu}"\s+label="{re.escape(action_label)}"',
                30,
                f"the {menu} menu ran {action_label!r}",
                begin,
            )
            return
        except TimeoutError:
            if attempt == 2:
                raise
            print(
                "  [RETRY] the {} menu's {!r} click missed; retrying once".format(
                    menu, label
                )
            )


def logged_scene_rect(session: EditorSession, view: int) -> tuple:
    """One scene viewport's last logged rectangle, visible flag included."""
    match = latest_log_match(session, SCENE_RECT_PATTERN, view, "scene view rect")
    return (
        float(match.group(2)),
        float(match.group(3)),
        float(match.group(4)),
        float(match.group(5)),
        match.group(6) == "true",
    )


def inspector_section_rect(session: EditorSession, component: str) -> tuple:
    """One Inspector section header's last logged rectangle."""
    matches = [
        match
        for match in re.finditer(INSPECTOR_SECTION_RECT_PATTERN, session.read_log())
        if match.group(1) == component
    ]
    if not matches:
        raise AssertionError(f"no Inspector section for {component!r} in the log")
    last = matches[-1]
    return (
        float(last.group(2)),
        float(last.group(3)),
        float(last.group(4)),
        float(last.group(5)),
    )


def inspector_add_rect(session: EditorSession) -> tuple:
    """The Add Component button's last logged rectangle."""
    matches = list(re.finditer(INSPECTOR_ADD_RECT_PATTERN, session.read_log()))
    if not matches:
        raise AssertionError("the Inspector's add button never reported its rectangle")
    last = matches[-1]
    return (
        float(last.group(1)),
        float(last.group(2)),
        float(last.group(3)),
        float(last.group(4)),
    )


def latest_detail_entity(session: EditorSession) -> str:
    """The entity the newest rows heartbeat reports as the Inspector's detail."""
    details = re.findall(r'detail="([^"-][^"]*)"', session.read_log())
    if not details:
        raise AssertionError("no rows heartbeat reported a selected entity")
    return details[-1]


def inspector_rows(session: EditorSession, component_suffix: str) -> list:
    """Every drawn Inspector row of the components whose name ends that way.

    Each entry is `(kind, component, rest)`: the row's kind, the component it
    belongs to, and the rest of the line (a field's name and widget, a
    header's text, a gap's height) for the caller to read.
    """
    return [
        (match.group(1), match.group(2), match.group(3))
        for match in re.finditer(INSPECTOR_ROW_PATTERN, session.read_log())
        if match.group(2).endswith(component_suffix)
    ]


def latest_detail_components(session: EditorSession) -> int:
    """How many components the newest rows heartbeat sees on the detail entity."""
    counts = re.findall(r"detail_components=(\d+)", session.read_log())
    if not counts:
        raise AssertionError("no rows heartbeat reported a component count")
    return int(counts[-1])


def wait_for_detail_components(session: EditorSession, target: int, label: str) -> None:
    """Waits until the newest rows heartbeat reports `target` component count."""
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        counts = re.findall(r"detail_components=(\d+)", session.read_log())
        if counts and int(counts[-1]) == target:
            print("  [OK] {}".format(label))
            return
        # Poll until a heartbeat with the expected count shows up.
        time.sleep(0.3)
    raise TimeoutError(
        "{}: the heartbeats never reported detail_components={}\n".format(label, target)
        + session.read_log()[-2000:]
    )


def console_dump(session: EditorSession, begin: int):
    """The newest console dump written since `begin`, or None when none is."""
    matches = list(re.finditer(CONSOLE_DUMP_PATTERN, session.read_log()[begin:]))
    return matches[-1] if matches else None


def wait_for_stub_record(session: EditorSession, timeout: float = 30.0) -> str:
    """Waits for the stub editor to record its arguments, and returns them."""
    record = session.stub_editor_record
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if record.is_file():
            text = record.read_text(encoding="utf-8").strip()
            if text:
                print("  [OK] the stub editor recorded {}".format(text))
                return text
        # The stub is a separate process writing a file: poll rather than
        # busy-wait, so the CPU stays free for the editor under test.
        time.sleep(0.2)
    raise TimeoutError(
        "the stub code editor recorded nothing at {}".format(record)
    )


def last_log_match(session: EditorSession, pattern: str, start: int = 0) -> re.Match:
    """The newest match of `pattern` in the log from `start`: a rectangle or a
    row can be logged again as the layout settles, and the latest one counts."""
    matches = list(re.finditer(pattern, session.read_log()[start:]))
    assert matches, "no match for {}".format(pattern)
    return matches[-1]


def logged_rect_centre(line: str) -> tuple:
    """The centre of a rectangle a log line spells as `min_x`..`max_y`."""
    values = {
        key: float(value)
        for key, value in re.findall(r"\b(min_x|min_y|max_x|max_y)=(-?[0-9.]+)", line)
    }
    return (
        (values["min_x"] + values["max_x"]) / 2.0,
        (values["min_y"] + values["max_y"]) / 2.0,
    )


def logged_rect_bounds(line: str) -> tuple:
    """A rectangle a log line spells as `min_x`..`max_y`, as (x, y, width, height)."""
    values = {
        key: float(value)
        for key, value in re.findall(r"\b(min_x|min_y|max_x|max_y)=(-?[0-9.]+)", line)
    }
    return (
        values["min_x"],
        values["min_y"],
        values["max_x"] - values["min_x"],
        values["max_y"] - values["min_y"],
    )


def rect_center(rect: tuple) -> tuple:
    """The center of an (x, y, width, height) rectangle."""
    return (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0)


def rect_contains(outer: tuple, inner: tuple) -> bool:
    """Whether `outer` holds `inner`, both (x, y, width, height), half a pixel slack."""
    return (
        inner[0] >= outer[0] - 0.5
        and inner[1] >= outer[1] - 0.5
        and inner[0] + inner[2] <= outer[0] + outer[2] + 0.5
        and inner[1] + inner[3] <= outer[1] + outer[3] + 0.5
    )


def wait_for_frames_past(session: EditorSession, highest: int, timeout: float, label: str) -> int:
    """Waits until a rows heartbeat reports a frame counter past `highest`.

    The counter continuing proves the shell's session resource - the dock and
    the context - survived whatever reload or patch rewrote the module.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        frames = rows_frames(session)
        beyond = [frame for frame in frames if frame > highest]
        if beyond:
            print("  [OK] {}".format(label))
            return max(beyond)
        # Poll until a heartbeat past the reload or patch shows up.
        time.sleep(0.5)
    raise TimeoutError(
        "{}: the row heartbeat did not continue past frame {}; the shell's "
        "state was reset\n".format(label, highest) + session.read_log()[-4000:]
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        type=Path,
        help="the built pill_editor binary (default: the debug build)",
    )
    parser.add_argument(
        "--project",
        default="../examples/italian_brainrot",
        help="the project, resolved the way the editor resolves PROJECT_PATH (from modules/)",
    )
    parser.add_argument("--timeout", type=int, default=300)
    arguments = parser.parse_args()
    # The egui editor needs the DirectComposition overlay, a Windows facility.
    if not EditorSession.supported():
        print("SKIP: the egui editor suite needs Windows")
        return 0
    # A concurrent cargo build *of this workspace* rewrites the shared dylibs
    # under the modules this suite loads, which turns a clean start into a
    # loader failure. Cargo running against another checkout cannot touch this
    # tree's dylibs, so those processes are ignored: a person may well have a
    # second project open while this suite runs.
    cargo = subprocess.run(
        [
            "powershell",
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name='cargo.exe'\""
            " | Select-Object -ExpandProperty CommandLine",
        ],
        capture_output=True,
        text=True,
    ).stdout
    workspace = str(REPOSITORY_ROOT).lower()
    running_here = [
        line
        for line in cargo.splitlines()
        if "modules" in line.lower() and workspace in line.lower()
    ]
    if running_here:
        raise SystemExit(
            "a cargo build of this workspace is running; wait for it to finish "
            "before running the editor suite - it rewrites the shared dylibs "
            "the modules load against:\n" + "\n".join(running_here)
        )
    # The default must be the egui binary: EditorSession's own default is the
    # native editor, whose binary is a different build and crashes at startup
    # against the egui editor's dylib split.
    binary = arguments.binary.resolve() if arguments.binary else EGUI_EDITOR_BINARY
    # Built by the gate, not by this suite, so it is verified rather than
    # trusted: a binary older than the sources it was compiled from would test
    # code that no longer exists and report green.
    refusal = verify_editor_binary(binary)
    if refusal:
        raise SystemExit(refusal)
    shell_source = (
        MODULES_ROOT
        / "extensions"
        / "pill_editor_shell_egui"
        / "pill_editor_shell_egui_editor"
        / "src"
        / "shell.rs"
    )
    # Byte-faithful reads (`newline=""`): `EditorSession.edit_source` writes
    # the text back as given, so a translated read would rewrite every line's
    # ending and the host's patch classifier would see a whole-file change
    # instead of the one body the step edits.
    with shell_source.open("r", encoding="utf-8", newline="") as file:
        original_source = file.read()
    # The Entities tab body lives in its own file, beside the root.
    tab_entities_source = shell_source.parent / "shell" / "tab_entities.rs"
    with tab_entities_source.open("r", encoding="utf-8", newline="") as file:
        original_tab_source = file.read()
    # The Systems tab's body has a file of its own too; its patch case proves
    # the module whose opening once reached only the sibling glob.
    tab_systems_source = shell_source.parent / "shell" / "tab_systems.rs"
    with tab_systems_source.open("r", encoding="utf-8", newline="") as file:
        original_systems_source = file.read()
    project_source = REPOSITORY_ROOT / "examples" / "italian_brainrot" / "src" / "scene.rs"
    # The Assets step's meta probe: a sourced file with no `.meta` sidecar
    # yet, staged before the editor starts so the res watcher never sees a
    # creation event for it - the write-meta action answers exactly this
    # state.
    asset_root = REPOSITORY_ROOT / "examples" / "italian_brainrot" / "res"
    stage_file = asset_root / "s16_stage.png"
    samples = sorted((asset_root / "textures").glob("*.png"))
    assert samples, "no texture png to stage the meta probe from"
    stage_file.write_bytes(samples[0].read_bytes())
    # The asset reference step (W-0048 Stage 3) needs a material file: the
    # example builds its materials in code, and the picker offers the files of
    # the field's type, so there must be one in `res` to pick and to drop. It
    # names the PBR chain's shader - installed in every build under a guid
    # derived from its name (`pbr_pipeline::SHADER_GUID`) - and binds no maps,
    # so it loads and draws in this project. Staged before the editor starts,
    # like the meta probe, so the res watcher sees it indexed at startup.
    reference_material = asset_root / "materials" / "s16_reference.material"
    reference_material.parent.mkdir(exist_ok=True)
    reference_material.write_text(
        json.dumps(
            {
                "format_version": 1,
                "asset_type": "pill_master_renderer::assets::Material",
                "guid": "5f16a9c2d47b4e0f8a1b2c3d4e5f6a7b",
                "asset": {
                    "shader": "8cedb4197c9f26a86708b1cd14399eab",
                    "textures": {},
                    "parameters": {
                        "pbr_base": {"Color": [1.0, 1.0, 1.0]},
                        "pbr_emissive": {"Color": [1.0, 1.0, 1.0]},
                        "pbr_metallic": {"Scalar": 1.0},
                        "pbr_roughness": {"Scalar": 1.0},
                    },
                    "rendering_order": 255,
                },
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    # The Inspector's settings step writes two `.meta` sidecars through the
    # real apply and undoes them; the engine's normalization reorders their
    # keys, so their original bytes are snapshotted and put back in the
    # finally block, the way the sources are.
    inspector_metas = [
        asset_root / "models" / "cube.obj.meta",
        asset_root / "textures" / "chimpanzini_bananini_color.png.meta",
    ]
    original_metas = {}
    for meta_path in inspector_metas:
        assert meta_path.is_file(), f"the settings step's sidecar is missing: {meta_path}"
        original_metas[meta_path] = meta_path.read_bytes()
    # The theme layers' files (W-0127 Stage 6) live under `res/editor/` and in
    # the project root; the example ships neither, so the step writes them and
    # the finally block takes them off the disk again.
    theme_root = asset_root / "editor"
    editor_settings_path = asset_root.parent / "editor_settings.yaml"
    # The phases wait for two row heartbeats, a scene-selection probe, a
    # gizmo drag phase, the assets pipeline and a patch compile; the
    # executable's seconds deadline is a hang guard, kept comfortably above
    # the phases even on a slow machine.
    session = EditorSession(
        "egui",
        "main",
        project=arguments.project,
        test_seconds=420,
        binary=binary,
    )
    # The Preferences step seeds the file before the editor starts: the
    # seeded cap proves the startup read of the live reader, and the
    # project's remembered entry keeps the tool at the defaults (translate,
    # world, unsnapped) so the earlier steps see the state they expect.
    session.write_appdata_json(
        "preferences.json",
        {
            "history_step_cap": 400,
            "snapping_steps": {
                "translation": 0.25,
                "rotation_degrees": 15.0,
                "scale": 0.1,
            },
            "gizmo": {
                "projects": {
                    "italian_brainrot": {
                        "mode": "translate",
                        "space": "world",
                        "snap": False,
                    }
                }
            },
        },
    )
    session.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "1"})
    try:
        # Step 1: startup, all the way to both tabs reporting rows.
        wait_for_token(session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "startup")
        wait_for_token(
            session,
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+surface=1\s+detached=false",
            120,
            "scene surface",
        )
        # Both scene viewports attach at startup: the Scene tab's surface
        # and the second viewport's.
        session.wait_occurrence(
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+surface=\d+\s+detached=false",
            2,
            30,
            "second scene surface",
        )
        wait_for_token(session, re.escape(EGUI_SHELL_LOADED_TOKEN), 180, "shell load")
        rows = wait_for_token(session, ROWS_PATTERN, 180, "rows heartbeat")
        match = re.search(r"entities=(\d+)\s+systems=(\d+)", rows)
        assert match and int(match.group(1)) > 0, f"no entities in the rows: {rows}"
        print(f"  [OK] shell loaded, {rows}")

        # A misspelled verb must be refused at once: the session raises on the
        # channel's rejection line instead of the suite waiting out its
        # timeout for an answer that never comes.
        started = time.monotonic()
        refusal = None
        try:
            session.request("nonsense-verb", "an answer that never comes", 30, "a refusal")
        except ScriptedCommandRejected as error:
            refusal = error
        elapsed = time.monotonic() - started
        assert refusal is not None, "the channel accepted a verb it does not know"
        assert refusal.command == "nonsense-verb", f"refused {refusal.command!r}, not the verb"
        assert "unknown verb" in refusal.reason, f"unexpected reason: {refusal.reason}"
        assert elapsed < 15, f"the refusal took {elapsed:.1f}s"
        print(f"  [OK] a misspelled verb was refused in {elapsed:.1f}s: {refusal.reason}")

        # The same rule for a line whose grammar is right and whose target is
        # not: a handle no entity answers to is refused at once, so a suite that
        # mistypes an entity sees the refusal instead of a step that times out
        # looking like a slow editor.
        started = time.monotonic()
        refusal = None
        try:
            session.request(
                "select entity 9v999",
                "an answer that never comes",
                30,
                "a target refusal",
            )
        except ScriptedCommandRejected as error:
            refusal = error
        elapsed = time.monotonic() - started
        assert refusal is not None, "the channel resolved an entity that is not there"
        assert refusal.command == "select entity 9v999", f"refused {refusal.command!r}"
        assert "9v999" in refusal.reason, f"unexpected reason: {refusal.reason}"
        assert elapsed < 15, f"the refusal took {elapsed:.1f}s"
        print(f"  [OK] an unresolvable entity was refused in {elapsed:.1f}s: {refusal.reason}")

        def field_values(entity: str, field: str) -> "list[float]":
            """One array field of `entity`, from a scripted `dump field`."""
            dump = session.request(
                f"dump field {entity} {TRANSFORM_COMPONENT} {field}",
                re.escape(FIELD_TOKEN),
                30,
                f"the {field} dump",
            )
            match = re.search(r"value=Array\(\[([^\]]*)\]\)", dump)
            assert match, "the {} dump carried no array: {}".format(field, dump[-500:])
            return [
                float(value) for value in re.findall(r"F32\(([-\d.eE+]+)\)", match.group(1))
            ]

        def wait_applied(since: int, label: str) -> None:
            """Polls until the editor applied a command batch past `since`."""
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                if occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN)) > since:
                    print(f"  [OK] {label}")
                    return
                # Poll until the queued batch crosses the frame boundary.
                time.sleep(0.2)
            raise TimeoutError(
                label + ": no command batch applied\n" + session.read_log()[-2000:]
            )

        # Step 2: scene selection - a click inside a scene viewport picks
        # through the renderer's pick contract, and the answer moves the
        # shared selection and publishes the outline; a corner click hits
        # nothing and clears both. The view's rectangle comes from the
        # frontend's own log, in the physical pixels the scripted clicks
        # use.
        scenes = session.request(
            "dump scenes", SCENE_RECT_PATTERN, 30, "the scene rects dumped"
        )
        scene_rect = next(
            (
                tuple(int(value) for value in match.groups()[1:5])
                for match in re.finditer(SCENE_RECT_PATTERN, scenes)
                if match.group(1) == "0" and match.group(6) == "true"
            ),
            None,
        )
        assert scene_rect, "no visible scene view rect in the log"
        scene_x, scene_y, scene_width, scene_height = scene_rect
        print(
            f"  [OK] scene view rect x={scene_x} y={scene_y} "
            f"w={scene_width} h={scene_height}"
        )

        # Probe a 3x3 grid; at least one cell must hit an entity. The
        # project's scene frames its entities, so the middle usually answers
        # first.
        picked_entity = None
        for fraction_x, fraction_y in (
            (0.5, 0.5),
            (0.35, 0.4),
            (0.65, 0.6),
            (0.5, 0.25),
            (0.5, 0.75),
            (0.25, 0.5),
            (0.75, 0.5),
            (0.2, 0.2),
            (0.8, 0.8),
        ):
            click_x = scene_x + scene_width * fraction_x
            click_y = scene_y + scene_height * fraction_y
            begin = len(session.read_log())
            session.send(f"click {click_x:.0f} {click_y:.0f}")
            found, _ = session.wait_either(
                [PICK_ENTITY_PATTERN, PICK_NOTHING_PATTERN], 120, begin
            )
            if found == PICK_ENTITY_PATTERN:
                match = re.search(PICK_ENTITY_PATTERN, session.read_log()[begin:])
                picked_entity = match.group(1)
                print(
                    f"  [OK] click at ({click_x:.0f}, {click_y:.0f}) picked "
                    f"{picked_entity}"
                )
                break
            print(f"  [OK] click at ({click_x:.0f}, {click_y:.0f}) hit nothing, next")
        assert picked_entity, "no grid point hit an entity in the scene view"
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the pick moved the shared selection",
        )
        session.wait(
            re.escape(OUTLINE_SET_TOKEN) + rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the outline was published for it",
        )

        # The dock's splitter (W-0049 Stage 6): the grab band beside a
        # panel's edge takes the drag, so a press a few pixels outside the
        # one-pixel line still moves the split - and the scene window, which
        # the shell places at the dock's own rectangle, moves with it. The
        # band is the line plus eight points of grab width on each side, so a
        # press four points above the scene's bottom edge is inside it and
        # was outside the four points the dock shipped with.
        badge_begin = len(session.read_log())
        splitter_x = scene_x + scene_width * 0.5
        splitter_y = scene_y + scene_height - 4.0
        # A hover on the band asks the dock for the resize cursor, which the
        # executable maps and logs on change - the half of the splitter's
        # story a drag cannot show.
        hover_begin = len(session.read_log())
        session.send(f"hover ui {splitter_x:.0f} {splitter_y:.0f}")
        session.wait(
            re.escape(CURSOR_ICON_TOKEN) + r'\s+icon="ResizeVertical"',
            30,
            "the resize cursor over the splitter",
            hover_begin,
        )
        print("  [OK] hovering the splitter asked for the resize cursor")
        session.send(f"drag {splitter_x:.0f} {splitter_y:.0f} {splitter_x:.0f} {splitter_y + 24:.0f}")
        # The release lands on the next frame and the dock lays out after it,
        # so the rects are read once that frame has had time to draw.
        time.sleep(0.6)
        scenes = session.request(
            "dump scenes", SCENE_RECT_PATTERN, 30, "the scene rects after the splitter drag"
        )
        moved = next(
            (
                tuple(int(value) for value in match.groups()[1:5])
                for match in re.finditer(SCENE_RECT_PATTERN, scenes)
                if match.group(1) == "0" and match.group(6) == "true"
            ),
            None,
        )
        assert moved is not None, "the scene rect vanished after the splitter drag"
        assert moved != scene_rect, (
            "a drag within the grab band left the split where it was: {} -> {}".format(
                scene_rect, moved
            )
        )
        print(
            "  [OK] a splitter drag {}px off the line moved the split {}".format(
                4, moved
            )
        )

        # The scene badges (W-0049 Stage 12): the executable projects every
        # entity into the view and the shell draws a chip at each point. A
        # click on a chip selects its entity through the shell, so no scene
        # pick is asked for - the badge's own log line is the proof. The chips
        # report their rectangles only when they move, and the splitter drag
        # above moved the window, so the lines read here are the live ones.
        session.wait(
            re.escape(SCENE_BADGE_RECT_TOKEN)
            + r"\s+view=\d+\s+entity=\S+\s+min_x=(-?[\d.]+)\s+min_y=(-?[\d.]+)"
            + r"\s+max_x=(-?[\d.]+)\s+max_y=(-?[\d.]+)",
            60,
            "a scene badge's rectangle",
            badge_begin,
        )
        badge = re.search(
            re.escape(SCENE_BADGE_RECT_TOKEN)
            + r"\s+view=\d+\s+entity=(\S+)\s+min_x=(-?[\d.]+)\s+min_y=(-?[\d.]+)"
            + r"\s+max_x=(-?[\d.]+)\s+max_y=(-?[\d.]+)",
            session.read_log()[badge_begin:],
        )
        assert badge, "the badge's rectangle carried no entity"
        badge_entity = badge.group(1)
        badge_x = (float(badge.group(2)) + float(badge.group(4))) * 0.5
        badge_y = (float(badge.group(3)) + float(badge.group(5))) * 0.5
        # Select another entity first, so the badge's own selection is a
        # change the log states rather than a repeat of the same handle.
        entities = re.findall(
            re.escape(ENTITY_ROW_TOKEN) + r'\s+display="([^"]+)"',
            session.request(
                "dump entities",
                re.escape(ENTITIES_DUMP_TOKEN),
                60,
                "the entity list for the badge step",
            ),
        )
        other = next(
            (entity for entity in entities if entity != badge_entity), None
        )
        if other is not None:
            begin = len(session.read_log())
            session.send("select entity {}".format(other))
            session.wait(
                re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(other)}",
                60,
                "another entity selected before the badge click",
                begin,
            )
        begin = len(session.read_log())
        session.send(f"click {badge_x:.0f} {badge_y:.0f}")
        session.wait(
            re.escape(SCENE_BADGE_TOKEN) + rf"\s+entity={re.escape(badge_entity)}",
            60,
            "the badge's click selected its entity",
            begin,
        )
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(badge_entity)}",
            60,
            "the badge's click moved the shared selection",
            begin,
        )
        print(f"  [OK] scene badge {badge_entity} clicked at ({badge_x:.0f}, {badge_y:.0f})")
        # The badge moved the shared selection; the steps below read the
        # entity the scene pick made, so it goes back on it here.
        restored = len(session.read_log())
        session.send(f"select entity {picked_entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the picked entity selected again",
            restored,
        )

        # The asset previews (W-0049 Stage 11): the preference turns them on,
        # the grid queues only the tiles it drew, and the texture's rendered
        # pixels come back as an image the tile draws. One request per frame,
        # so the assertions are the request line and then the finished image.
        session.send("edit preference editor.render_asset_previews true")
        session.send("browser textures")
        session.send("tab assets")
        preview_begin = len(session.read_log())
        session.wait(
            re.escape(PREVIEW_REQUESTED_TOKEN) + r'\s+path="[^"]+\.png"',
            120,
            "a texture tile's preview request",
            preview_begin,
        )
        session.wait(
            re.escape(PREVIEW_READY_TOKEN) + r'\s+path="[^"]+\.png"\s+width=128\s+height=128',
            120,
            "the texture tile's preview image",
            preview_begin,
        )
        print("  [OK] a texture tile's preview came back rendered")
        # A mesh tile takes the preview's own mesh pass: its own pipeline,
        # depth target and camera framing, so a mesh needs no material.
        session.send("browser models")
        mesh_begin = len(session.read_log())
        session.wait(
            re.escape(PREVIEW_REQUESTED_TOKEN) + r'\s+path="[^"]+\.obj"',
            120,
            "a mesh tile's preview request",
            mesh_begin,
        )
        session.wait(
            re.escape(PREVIEW_READY_TOKEN) + r'\s+path="[^"]+\.obj"\s+width=128\s+height=128',
            120,
            "the mesh tile's preview image",
            mesh_begin,
        )
        print("  [OK] a mesh tile's preview came back rendered")
        # Off again, so the rest of the run costs no preview work, and the
        # Inspector goes back on top of the column the grid shared, whose rows
        # the steps below read.
        session.send("edit preference editor.render_asset_previews false")
        session.send("tab inspector")

        # The Inspector's data follows the selection: the rows heartbeat
        # reports the detail tier (the selected entity and its component
        # count), which is what the Inspector tab draws.
        detail_pattern = (
            re.escape(EGUI_ROWS_TOKEN)
            + rf'\s+frames=\d+[^\n]*detail="{re.escape(picked_entity)}"'
            + r"\s+detail_components=(\d+)"
        )
        begin = len(session.read_log())
        session.wait(detail_pattern, 60, "the inspector reported the detail", begin)
        match = session.find(detail_pattern, begin)
        assert int(match.group(1)) >= 1, (
            "the inspector saw no components: {}".format(match.group(0))
        )
        print(f"  [OK] inspector detail {picked_entity} components={match.group(1)}")

        # The Inspector's rows are editable (Stage 17): a scripted `edit
        # array` mirrors a DragValue edit on the entity's component fields.
        # The command applies at the frame boundary, pushes one history
        # step whose label names the field path, and the world dump sees
        # the new value; undo puts the old one back.
        translation_before = field_values(picked_entity, "translation")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(
            f"edit array {picked_entity} {TRANSFORM_COMPONENT} translation 0 4.25"
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="set TransformComponent\[0\]\.translation"',
            30,
            "the inspector edit pushed one history step",
            begin,
        )
        wait_applied(applied_before, "inspector edit applied")
        assert field_values(picked_entity, "translation")[0] == 4.25, (
            "the inspector edit did not reach the world"
        )
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN)
            + r'\s+label="set TransformComponent\[0\]\.translation"',
            30,
            "the inspector edit undid",
            begin,
        )
        wait_applied(applied_before, "inspector undo applied")
        assert field_values(picked_entity, "translation") == translation_before, (
            "the undo did not restore the field"
        )
        print("  [OK] inspector edit and undo round-trip")

        # The corner of the view holds nothing, which clears both.
        begin = len(session.read_log())
        session.send(f"click {scene_x + 6} {scene_y + 6}")
        session.wait_token(SCENE_PICK_NOTHING_TOKEN, 120, "the corner hits nothing", begin)
        session.wait_token(OUTLINE_CLEARED_TOKEN, 60, "the outline cleared with it", begin)
        print("  [OK] selection and outline cleared")

        # Step 3: the gizmo - the selection grows translate handles, and
        # scripted drags on each tool's own handle move, turn and scale the
        # entity through the real pick; the world is checked directly with
        # `dump field`.
        begin = len(session.read_log())
        session.send("click 640 317")
        found, _ = session.wait_either(
            [PICK_ENTITY_PATTERN, PICK_NOTHING_PATTERN], 120, begin
        )
        assert found == PICK_ENTITY_PATTERN, "the gizmo step could not re-select the entity"
        gizmo_entity = re.search(PICK_ENTITY_PATTERN, session.read_log()[begin:]).group(1)
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN)
            + rf'\s+entity={re.escape(gizmo_entity)}'
            + r'\s+mode="translate"\s+space="world"',
            60,
            "the translate handles are drawn for the selection",
            begin,
        )

        # Translate: drag the X arrow 120 px right; the entity's x moves.
        before_x = field_values(gizmo_entity, "translation")[0]
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("gizmo drag x 120 0")
        session.wait(
            re.escape(GIZMO_DRAG_PRESS_TOKEN) + r'\s+axis="x"',
            30,
            "the press ray was aimed at the X handle",
            begin,
        )
        session.wait(
            re.escape(SCENE_PICK_HANDLE_TOKEN) + r"\s+handle=\d+",
            60,
            "the pick answered a handle",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN)
            + rf'\s+mode="translate"\s+axis="x"\s+entity={re.escape(gizmo_entity)}',
            60,
            "the drag started on the translate X handle",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r'\s+value=\[([-\d.eE+]+),[^\n]*snapped=false',
            60,
            "the drag ended, unsnapped, with a value",
            begin,
        )
        wait_applied(applied_before, "the drag's step applied")
        after_x = field_values(gizmo_entity, "translation")[0]
        assert abs(after_x - before_x) > 0.5, (
            f"the translate drag barely moved the entity: {before_x} -> {after_x}"
        )
        print(f"  [OK] translate drag moved x {before_x:.3f} -> {after_x:.3f}")

        # A released drag is one recorded step: undo puts the press's
        # translation back and redo returns the drag's, so the one
        # manipulation a person is most likely to undo is undoable.
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="gizmo translate"',
            30,
            "the drag undid as one step",
            begin,
        )
        wait_applied(applied_before, "the drag's undo applied")
        undone_x = field_values(gizmo_entity, "translation")[0]
        assert abs(undone_x - before_x) < 1e-3, (
            f"the undo did not restore the press's x: {undone_x} vs {before_x}"
        )
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.redo")
        session.wait(
            re.escape(HISTORY_REDO_TOKEN) + r'\s+label="gizmo translate"',
            30,
            "the drag redid",
            begin,
        )
        wait_applied(applied_before, "the drag's redo applied")
        redone_x = field_values(gizmo_entity, "translation")[0]
        assert abs(redone_x - after_x) < 1e-3, (
            f"the redo did not return the drag's x: {redone_x} vs {after_x}"
        )
        print(f"  [OK] drag undo/redo round-trip x {undone_x:.3f} -> {redone_x:.3f}")

        # Rotate: the Y ring turns the entity; the ended token carries the
        # quaternion the drag wrote. The project's own system animates this
        # entity's rotation, so a later world dump is the system's value,
        # not the drag's - the written quaternion itself is asserted (a
        # unit quaternion), like the native suite's ring step.
        begin = len(session.read_log())
        session.send("gizmo rotate")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r'\s+mode="rotate"',
            30,
            "the rotate tool is active",
            begin,
        )
        begin = len(session.read_log())
        session.send("gizmo drag y 120 0")
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN)
            + rf'\s+mode="rotate"\s+axis="y"\s+entity={re.escape(gizmo_entity)}',
            60,
            "the drag started on the rotate Y ring",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="rotate"\s+axis="y"\s+entity=\S+'
            r"\s+value=\[[-\d.eE+, ]+\]",
            60,
            "the ring drag ended with the quaternion it wrote",
            begin,
        )
        match = re.search(r"value=\[([-\d.eE+, ]+)\]", session.read_log()[begin:])
        quaternion = [float(value) for value in match.group(1).split(",")]
        length = sum(component * component for component in quaternion) ** 0.5
        assert abs(length - 1.0) < 0.01, (
            f"the written quaternion is not unit length: {quaternion}"
        )
        print(f"  [OK] rotate drag wrote the quaternion {quaternion}")

        # Scale: the X cube resizes the entity along its own axis; the
        # factor the drag wrote is asserted from the ended token, and the
        # world value is printed for the record.
        begin = len(session.read_log())
        session.send("gizmo scale")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r'\s+mode="scale"',
            30,
            "the scale tool is active",
            begin,
        )
        begin = len(session.read_log())
        session.send("gizmo drag x 120 0")
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN)
            + rf'\s+mode="scale"\s+axis="x"\s+entity={re.escape(gizmo_entity)}',
            60,
            "the drag started on the scale X handle",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="scale"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),[-\d.eE+, ]*\]\s+snapped=false",
            60,
            "the scale drag ended with the value it wrote",
            begin,
        )
        match = re.search(r"value=\[([-\d.eE+]+),", session.read_log()[begin:])
        factor = float(match.group(1))
        assert abs(factor - 1.0) > 0.01, f"the scale drag wrote no change: {factor}"
        scale = field_values(gizmo_entity, "scale")
        print(f"  [OK] scale drag wrote {factor:.3f} (scale now {scale})")

        # Step 3a: a keyboard chord reaches the same handler a real press does -
        # the E shortcut picks the rotate tool, which the scale tool step left
        # behind - and the injected chord is logged, so the injection itself is
        # provable. The next step's `gizmo translate` still changes the tool,
        # so it logs the way it always did.
        begin = len(session.read_log())
        session.send("key e")
        session.wait(
            re.escape(KEY_INJECTED_TOKEN) + r'\s+chord="e"',
            30,
            "the key chord injected",
            begin,
        )
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r'\s+mode="rotate"',
            30,
            "the key shortcut switched the tool",
            begin,
        )
        print("  [OK] a key chord reached the tool shortcut")

        # Step 3a2: the keybindings file rebinds a shortcut without a restart,
        # and a chord two commands share is reported by both of their names
        # (W-0038 Stage 2). The file lands in the editor's state folder; the
        # watcher applies it on the next frame.
        begin = len(session.read_log())
        session.write_keybindings(
            {
                "preset": "unity",
                "overrides": {
                    "editor.tool_translate": ["t"],
                    "editor.tool_rotate": ["t"],
                },
            }
        )
        session.wait(
            re.escape(EDITOR_KEYBINDINGS_APPLIED_TOKEN) + r"\s+preset=unity\s+overrides=2",
            30,
            "the keybindings file applied",
            begin,
        )
        conflict = re.search(
            re.escape(EDITOR_KEYBINDING_CONFLICT_TOKEN)
            + r'\s+chord="t"\s+commands="([^"]+)"',
            session.read_log()[begin:],
        )
        assert conflict, "the rebinding reported no conflicting chord"
        assert "editor.tool_translate" in conflict.group(1)
        assert "editor.tool_rotate" in conflict.group(1)
        # The rebound chord resolves through the registry: `t` runs translate
        # (the first command in registration order wins a shared chord).
        session.send("key t")
        session.wait(
            re.escape(EDITOR_SHORTCUT_RAN_TOKEN) + r'\s+command="editor\.tool_translate"',
            30,
            "the rebound chord ran its command",
            begin,
        )
        print("  [OK] a keybindings override rebound a shortcut live")

        # The conflict is also a warning toast (W-0038 Stage 4), and clicking
        # that toast never reaches the scene: the row's rectangle joins the
        # frame's overlay regions.
        session.wait(
            re.escape(TOAST_TOKEN)
            + r'\s+level="warning"[^\n]*text="Keybinding conflict:[^"]*',
            30,
            "the conflict raised a warning toast",
            begin,
        )
        toast_rows = [
            match
            for match in re.finditer(TOAST_LINE_PATTERN, session.read_log()[begin:])
            if match.group(1) == "warning" and "Keybinding conflict" in match.group(2)
        ]
        assert toast_rows, "the warning toast logged no rectangle"
        row = toast_rows[-1]
        toast_center = (
            float(row.group(3)) + float(row.group(5)) / 2.0,
            float(row.group(4)) + float(row.group(6)) / 2.0,
        )
        pick_start = len(session.read_log())
        session.send(f"click {toast_center[0]:.0f} {toast_center[1]:.0f}")
        # The rows heartbeat (every 210 frames) proves a frame handled the
        # click before the absence of a pick line is read as an answer.
        session.wait(
            re.escape(EGUI_ROWS_TOKEN), 30, "a frame ran after the toast click", pick_start
        )
        assert "scene pick" not in session.read_log()[pick_start:], (
            "a click on a toast reached the scene"
        )
        print("  [OK] a toast click stayed in the UI")

        # Step 3a3: the command palette (W-0038 Stage 3). The scripted channel
        # opens it with a query, the shell filters the registry's commands, and
        # a run takes the menu bar's own action path - the same log token, with
        # `menu="palette"` naming the surface that asked.
        begin = len(session.read_log())
        session.send("palette translate")
        session.wait(
            re.escape(PALETTE_QUERY_TOKEN) + r'\s+query="translate"\s+matches=[1-9][0-9]*',
            30,
            "the palette filtered the registry's commands",
            begin,
        )
        filtered = re.search(
            re.escape(PALETTE_QUERY_TOKEN)
            + r'\s+query="translate"\s+matches=(\d+)\s+first="([^"]+)"',
            session.read_log()[begin:],
        )
        assert filtered, "the palette's filter line carried no match count"
        assert filtered.group(2) == "Translate", (
            "the palette's first match was {!r}, not the translate command".format(
                filtered.group(2)
            )
        )
        session.send("palette run")
        session.wait(
            re.escape(MENU_ACTION_TOKEN) + r'\s+menu="palette"\s+label="Translate"',
            30,
            "the palette ran its selected command",
            begin,
        )
        session.wait(
            re.escape(PALETTE_CLOSED_TOKEN), 30, "the palette closed after running", begin
        )
        print("  [OK] the palette filtered and ran a command")
        # A second open, closed without running: the popup must not stay over
        # the viewport, where every later click would be the UI's.
        begin = len(session.read_log())
        session.send("palette scale")
        session.wait(
            re.escape(PALETTE_QUERY_TOKEN) + r'\s+query="scale"',
            30,
            "the palette reopened on a second query",
            begin,
        )
        session.send("palette close")
        session.wait(
            re.escape(PALETTE_DISMISSED_TOKEN), 30, "the palette closed on request", begin
        )
        print("  [OK] the palette closed without running")

        # A registered command this build has no action for must not be offered:
        # the layout commands lost their menu path with the Layout menu that the
        # bar never had, so the palette names each one and lists no row. Before
        # this the row appeared, a run closed the popup, and nothing happened.
        begin = len(session.read_log())
        session.send("palette save layout")
        session.wait(
            re.escape(COMMAND_NO_ACTION_TOKEN) + r'\s+command="editor\.save_layout"',
            30,
            "the palette named the command it cannot run",
            begin,
        )
        session.wait(
            re.escape(PALETTE_QUERY_TOKEN) + r'\s+query="save layout"\s+matches=0',
            30,
            "the palette listed no command without an action",
            begin,
        )
        session.send("palette close")
        session.wait(
            re.escape(PALETTE_DISMISSED_TOKEN), 30, "the palette closed on request", begin
        )
        print("  [OK] the palette named and dropped a command with no action")

        # Step 3a4: the Console panel (W-0038 Stage 5). The tab reads the
        # telemetry capture ring, so the project's own log line is in the game
        # channel; the filters narrow what it draws.
        session.send("tab console")
        begin = len(session.read_log())
        session.send("dump console")
        session.wait(re.escape(CONSOLE_DUMP_TOKEN), 30, "the console dump", begin)
        dump = console_dump(session, begin)
        assert dump, "the console dump did not parse"
        assert int(dump.group("game")) >= 1, (
            "the project's own log line did not reach the game channel: "
            + dump.group(0)
        )
        assert int(dump.group("visible")) >= 1, "the console showed no rows"
        # A search narrows the rows to the project's lines: the token only
        # appears in them.
        begin = len(session.read_log())
        session.send("console search 12xxsees")
        session.wait(re.escape(CONSOLE_SEARCH_TOKEN), 30, "the console search applied", begin)
        # The filter lands on the executable's next frame, so a frame has to
        # run before the dump can read it.
        session.wait(re.escape(EGUI_ROWS_TOKEN), 60, "a frame applied the search", begin)
        begin = len(session.read_log())
        session.send("dump console")
        session.wait(re.escape(CONSOLE_DUMP_TOKEN), 30, "the searched console dump", begin)
        filtered = console_dump(session, begin)
        assert filtered, "the filtered console dump did not parse"
        assert filtered.group("search") == "12xxsees", (
            "the search did not reach the console: " + filtered.group(0)
        )
        assert int(filtered.group("visible")) == int(filtered.group("game")), (
            "the search kept engine rows: " + filtered.group(0)
        )
        # A level filter above the project's line leaves nothing.
        begin = len(session.read_log())
        session.send("console filter warn")
        session.wait(re.escape(CONSOLE_FILTER_TOKEN), 30, "the console filter applied", begin)
        session.wait(re.escape(EGUI_ROWS_TOKEN), 60, "a frame applied the level", begin)
        begin = len(session.read_log())
        session.send("dump console")
        session.wait(re.escape(CONSOLE_DUMP_TOKEN), 30, "the levelled console dump", begin)
        levelled = console_dump(session, begin)
        assert levelled, "the level-filtered console dump did not parse"
        assert levelled.group("level") == "warn", (
            "the level filter did not reach the console: " + levelled.group(0)
        )
        assert int(levelled.group("visible")) == 0, (
            "a warn filter kept info rows: " + levelled.group(0)
        )
        # Back to the unfiltered view, so the rest of the run reads the tab
        # the way a fresh session would.
        session.send('console search ""')
        session.send("console filter info")
        session.wait(re.escape(CONSOLE_FILTER_TOKEN), 30, "the console filter reset", begin)
        print("  [OK] the console listed, searched and filtered the log")

        # Step 3a5: the Statistics tab (W-0038 Stage 7). The shell times every
        # tab body and its own pass, so the tab's rows are measurements - and
        # the tabs this suite has opened already must have non-zero times.
        stats_begin = len(session.read_log())
        session.send("tab statistics")
        session.send("dump stats")
        session.wait(re.escape(STATS_DUMP_TOKEN), 30, "the statistics dump", stats_begin)
        measured = {
            match.group(1): float(match.group(2))
            for match in re.finditer(
                re.escape(STATS_TAB_TOKEN)
                + r'\s+name="(\w+)"\s+us=([0-9.]+)\s+passes=\d+',
                session.read_log()[stats_begin:],
            )
        }
        assert measured, "the Statistics tab measured no tab body"
        # The console tab was actively drawn for the whole console step, so it
        # is the one row whose non-zero time is guaranteed; a dock leaf draws
        # only its active tab, so the others depend on activation order.
        assert measured.get("console", 0.0) > 0.0, (
            "the Statistics tab showed no time for the console tab: {}".format(measured)
        )
        assert len(measured) >= 4, (
            "the Statistics tab measured too few tabs: {}".format(measured)
        )
        print(
            "  [OK] the Statistics tab timed {} drawn tabs".format(len(measured))
        )

        # Step 3a7: world resources (W-0038 Stage 14). The Inspector lists the
        # editable resources, a selection of one is recorded, and a field edit
        # applies and undoes like any other edit.
        begin = len(session.read_log())
        session.send("select resource {}".format(GIZMO_RESOURCE_NAME))
        session.wait(
            re.escape(RESOURCE_SELECTED_TOKEN)
            + r'\s+resource="{}'.format(re.escape(GIZMO_RESOURCE_NAME)),
            30,
            "the resource selection",
            begin,
        )
        dump = session.request(
            "dump resources", re.escape(RESOURCES_DUMP_TOKEN), 30, "the resources"
        )
        assert re.search(r"count=1\b", dump), (
            "the editable resources did not list: {}".format(dump[-600:])
        )
        assert re.search(
            re.escape(RESOURCE_FIELD_TOKEN)
            + r'\s+resource="[^"]+"\s+field="settings\.scale_step"\s+label="Scale step"\s+value="0\.1"',
            dump,
        ), "the seeded scale step is not the resource's value: {}".format(dump[-800:])
        begin = len(session.read_log())
        session.send(
            "edit resource {} settings.scale_step 0.25".format(GIZMO_RESOURCE_NAME)
        )
        session.wait(
            re.escape(RESOURCE_EDIT_TOKEN), 30, "the resource edit", begin
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="set {}\.settings\.scale_step"'.format(
                re.escape(GIZMO_RESOURCE_NAME)
            ),
            30,
            "the resource edit recorded a step",
            begin,
        )
        dump = session.request(
            "dump resources", re.escape(RESOURCES_DUMP_TOKEN), 30, "the edited resource"
        )
        assert re.search(
            re.escape(RESOURCE_FIELD_TOKEN)
            + r'\s+resource="[^"]+"\s+field="settings\.scale_step"\s+label="Scale step"\s+value="0\.25"',
            dump,
        ), "the resource edit did not apply: {}".format(dump[-800:])
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN)
            + r'\s+label="set {}\.settings\.scale_step"'.format(
                re.escape(GIZMO_RESOURCE_NAME)
            ),
            30,
            "the resource edit's undo",
            begin,
        )
        dump = session.request(
            "dump resources", re.escape(RESOURCES_DUMP_TOKEN), 30, "the restored resource"
        )
        assert re.search(
            re.escape(RESOURCE_FIELD_TOKEN)
            + r'\s+resource="[^"]+"\s+field="settings\.scale_step"\s+label="Scale step"\s+value="0\.1"',
            dump,
        ), "the undo did not restore the resource field: {}".format(dump[-800:])
        print("  [OK] a resource field edit applied and undid")
        # The selection itself is a recorded command: undoing it restores the
        # whole selection, entity slot included, rather than leaving the
        # Inspector on nothing.
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(SELECTION_RESTORED_TOKEN)
            + r'\s+entity="(?!none)[^"]+"\s+resource="none"',
            30,
            "the resource selection's undo",
            begin,
        )
        print("  [OK] a resource selection round-trips through undo")

        # Step 3a8: collection fields (W-0038 Stage 15). An array of structs
        # expands to one row per leaf, so an element of a collection field is
        # an ordinary field - it edits, records and undoes exactly like a float.
        dump = session.request(
            "dump entities", re.escape(ENTITIES_DUMP_TOKEN), 30, "the entity list"
        )
        showcase_row = next(
            (
                match
                for match in re.finditer(
                    re.escape(ENTITY_ROW_TOKEN)
                    + r'\s+display="([^"]+)"\s+components="([^"]*)"',
                    dump,
                )
                if SHOWCASE_COMPONENT in match.group(2)
            ),
            None,
        )
        assert showcase_row is not None, (
            "no entity carries the showcase component: {}".format(dump[-800:])
        )
        showcase_entity = showcase_row.group(1)
        leaf = "points[0].x"
        begin = len(session.read_log())
        session.send(
            "dump field {} {} {}".format(showcase_entity, SHOWCASE_COMPONENT, leaf)
        )
        session.wait(re.escape(FIELD_TOKEN), 30, "the collection leaf", begin)
        before = re.search(r"value=(?:F32\()?(-?[0-9.]+)", session.read_log()[begin:])
        assert before is not None, (
            "the element's leaf did not read back: {}".format(
                session.read_log()[begin:][-600:]
            )
        )
        original = float(before.group(1))
        begin = len(session.read_log())
        session.send(
            "edit set {} {} {} 2.5".format(showcase_entity, SHOWCASE_COMPONENT, leaf)
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="set {}[^"]*"'.format(re.escape(SHOWCASE_COMPONENT)),
            30,
            "the collection edit recorded a step",
            begin,
        )
        begin = len(session.read_log())
        session.send(
            "dump field {} {} {}".format(showcase_entity, SHOWCASE_COMPONENT, leaf)
        )
        session.wait(re.escape(FIELD_TOKEN), 30, "the edited leaf", begin)
        assert "2.5" in session.read_log()[begin:], (
            "the collection edit did not apply: {}".format(
                session.read_log()[begin:][-600:]
            )
        )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the collection undo", begin)
        begin = len(session.read_log())
        session.send(
            "dump field {} {} {}".format(showcase_entity, SHOWCASE_COMPONENT, leaf)
        )
        session.wait(re.escape(FIELD_TOKEN), 30, "the restored leaf", begin)
        restored = re.search(r"value=(?:F32\()?(-?[0-9.]+)", session.read_log()[begin:])
        assert restored is not None and float(restored.group(1)) == original, (
            "the undo did not restore the element: {}".format(
                session.read_log()[begin:][-600:]
            )
        )
        print("  [OK] a struct-array element edited and undid")

        # Step 3a8b: asset reference rows (W-0048 Stage 3). A mesh renderer's
        # mesh and material fields are asset rows, each naming the asset it
        # holds. A scripted pick points one at another asset and loads it on
        # demand - the material file the suite stages is loaded by nothing
        # before the pick - undo brings the previous asset back, `none` clears
        # the row, and a material tile dragged from the Assets tab onto the
        # material row assigns it. Both rows end on the assets they started
        # with.
        dump = session.request(
            "dump entities", re.escape(ENTITIES_DUMP_TOKEN), 30, "the entity list"
        )
        mesh_row = next(
            (
                match
                for match in re.finditer(
                    re.escape(ENTITY_ROW_TOKEN)
                    + r'\s+display="([^"]+)"\s+components="([^"]*)"',
                    dump,
                )
                if "MeshRendererComponent" in match.group(2)
            ),
            None,
        )
        assert mesh_row is not None, (
            "no entity carries a mesh renderer: {}".format(dump[-800:])
        )
        mesh_entity = mesh_row.group(1)
        # What was selected before this step: the viewport steps after it drag
        # the selection's gizmo, so the step puts the selection back.
        prior = session.request(
            "dump selection", re.escape(SELECTION_DUMP_TOKEN), 30, "the prior selection"
        )
        prior_match = re.search(r'primary="([^"]+)"', prior)
        assert prior_match is not None, "the prior selection did not read back: {}".format(
            prior[-400:]
        )
        prior_entity = prior_match.group(1)

        def asset_row_capture(field: str) -> str:
            """The selected entity's asset row for `field`, capturing its text."""
            return (
                re.escape(REFERENCE_ROW_TOKEN)
                + r"(?=[^\n]*entity="
                + re.escape(mesh_entity)
                + r"\b)"
                + r'(?=[^\n]*field="'
                + field
                + r'")[^\n]*asset="([^"]*)"'
            )

        def asset_row_pattern(field: str, asset: str) -> str:
            """The selected entity's asset row for `field`, naming `asset`."""
            return (
                re.escape(REFERENCE_ROW_TOKEN)
                + r"(?=[^\n]*entity="
                + re.escape(mesh_entity)
                + r"\b)"
                + r'(?=[^\n]*field="'
                + field
                + r'")[^\n]*asset="'
                + re.escape(asset)
                + r'"'
            )

        begin = len(session.read_log())
        session.send("select entity {}".format(mesh_entity))
        session.wait(asset_row_capture("mesh"), 30, "the mesh field's asset row", begin)
        session.wait(
            asset_row_capture("material"), 30, "the material field's asset row", begin
        )
        original_mesh = last_log_match(session, asset_row_capture("mesh"), begin).group(1)
        original_material = last_log_match(
            session, asset_row_capture("material"), begin
        ).group(1)
        assert original_mesh not in ("None", "(missing)"), (
            "the mesh row starts empty: {}".format(original_mesh)
        )
        assert original_material not in ("None", "(missing)"), (
            "the material row starts empty: {}".format(original_material)
        )
        print(
            "  [OK] the mesh renderer's rows name {} and {}".format(
                original_mesh, original_material
            )
        )

        # The pick: by path, through the recorded step the picker queues. The
        # mesh is not loaded before this, so the step loads it.
        begin = len(session.read_log())
        session.send(
            "edit set {} MeshRendererComponent mesh models/plane.obj".format(mesh_entity)
        )
        session.wait(
            re.escape(REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="mesh")[^\n]*asset="models/plane\.obj"',
            60,
            "the mesh pick",
            begin,
        )
        session.wait(
            asset_row_pattern("mesh", "models/plane.obj"),
            30,
            "the row names the picked mesh",
            begin,
        )
        print("  [OK] picking another mesh loaded and assigned it")

        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the pick's undo", begin)
        session.wait(
            asset_row_pattern("mesh", original_mesh),
            30,
            "the row names the original mesh again",
            begin,
        )
        print("  [OK] undo restored the original mesh")

        begin = len(session.read_log())
        session.send("edit set {} MeshRendererComponent mesh none".format(mesh_entity))
        session.wait(
            asset_row_pattern("mesh", "None"), 30, "the cleared reference", begin
        )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the clear's undo", begin)
        session.wait(
            asset_row_pattern("mesh", original_mesh),
            30,
            "the mesh row restored after the clear",
            begin,
        )
        print("  [OK] the reference cleared and came back")

        # The material pick: the staged file is indexed but loaded by nothing,
        # so assigning it is what loads it.
        staged_material = "materials/s16_reference.material"
        begin = len(session.read_log())
        session.send(
            "edit set {} MeshRendererComponent material {}".format(
                mesh_entity, staged_material
            )
        )
        session.wait(
            re.escape(REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="material")[^\n]*asset="'
            + re.escape(staged_material)
            + r'"',
            60,
            "the material pick",
            begin,
        )
        session.wait(
            asset_row_pattern("material", staged_material),
            30,
            "the row names the picked material",
            begin,
        )
        print("  [OK] picking the staged, unloaded material loaded and assigned it")

        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the material pick's undo", begin)
        session.wait(
            asset_row_pattern("material", original_material),
            30,
            "the row names the original material again",
            begin,
        )
        print("  [OK] undo restored the original material")

        # The drop: the material's tile dragged from the Assets tab onto the
        # material row. The Assets tab is brought to the front first: the grid
        # only draws - and so only logs its tiles - while it is visible.
        begin = len(session.read_log())
        session.send("tab assets")
        session.send("browser materials")
        tile_pattern = (
            re.escape(REFERENCE_TILE_RECT_TOKEN)
            + r'(?=[^\n]*path="'
            + re.escape(staged_material)
            + r'")[^\n]*'
        )
        session.wait(tile_pattern, 30, "the material tile's rectangle", begin)
        # The Inspector's body is shorter than its rows, and the mesh renderer
        # sits below others: the sections above it are collapsed, topmost
        # first, as a person collapsing a header does, until its material row's
        # rectangle - logged only while the row's centre is visible - comes on
        # screen. Which sections those are is read from the log rather than
        # listed here, so a component the project adds moves nothing.
        row_rect_pattern = (
            re.escape(REFERENCE_ROW_RECT_TOKEN)
            + r'(?=[^\n]*MeshRendererComponent\.material")[^\n]*'
        )
        visible_from = len(session.read_log())
        collapsed_sections = []
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if re.search(row_rect_pattern, session.read_log()[visible_from:]):
                break
            headers = {}
            # Only the sections the panel drew for the entity it shows now:
            # the shell forgets a previous subject's rectangles, so everything
            # after the last selection change is this entity's.
            log_text = session.read_log()
            last_change = log_text.rfind(SELECTION_CHANGED_TOKEN)
            scoped = log_text[last_change:] if last_change >= 0 else log_text
            for match in re.finditer(INSPECTOR_SECTION_RECT_PATTERN, scoped):
                headers[match.group(1)] = match
            above = sorted(
                (
                    (name, float(logged.group(3)))
                    for name, logged in headers.items()
                    if name != "pill_master_renderer::component::MeshRendererComponent"
                    and name not in collapsed_sections
                ),
                key=lambda entry: entry[1],
            )
            assert above, (
                "every Inspector section is collapsed and the material row's "
                "rectangle never came on screen"
            )
            component = above[0][0]
            begin = len(session.read_log())
            x, y, width, height = inspector_section_rect(session, component)
            session.send("click {:.0f} {:.0f}".format(x + 12.0, y + 6.0))
            session.wait(
                re.escape(INSPECTOR_SECTION_TOKEN)
                + r'(?=[^\n]*component="'
                + re.escape(component)
                + r'")[^\n]*collapsed=true',
                30,
                "the {} section collapsed".format(component.rsplit("::", 1)[-1]),
                begin,
            )
            collapsed_sections.append(component)
        session.wait(row_rect_pattern, 30, "the material row's rectangle", visible_from)
        row_rect = logged_rect_bounds(
            last_log_match(session, row_rect_pattern, visible_from).group(0)
        )
        print("  [OK] the material row came on screen")
        tile_x, tile_y = logged_rect_centre(last_log_match(session, tile_pattern).group(0))
        row_x, row_y = rect_center(row_rect)
        begin = len(session.read_log())
        session.send(f"drag {tile_x:.0f} {tile_y:.0f} {row_x:.0f} {row_y:.0f}")
        session.wait(
            re.escape(REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="material")[^\n]*asset="'
            + re.escape(staged_material)
            + r'"',
            60,
            "the dropped material",
            begin,
        )
        session.wait(
            asset_row_pattern("material", staged_material),
            30,
            "the row names the dropped material",
            begin,
        )
        print("  [OK] dropping a material tile onto the row assigned it")

        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the drop's undo", begin)
        session.wait(
            asset_row_pattern("material", original_material),
            30,
            "the original material after the undo",
            begin,
        )
        # The sections are opened again - each header is on screen when its
        # turn comes - so the steps after this see the Inspector as they did.
        for component in reversed(collapsed_sections):
            begin = len(session.read_log())
            x, y, width, height = inspector_section_rect(session, component)
            session.send("click {:.0f} {:.0f}".format(x + 12.0, y + 6.0))
            session.wait(
                re.escape(INSPECTOR_SECTION_TOKEN)
                + r'(?=[^\n]*component="'
                + re.escape(component)
                + r'")[^\n]*collapsed=false',
                30,
                "the {} section expanded".format(component.rsplit("::", 1)[-1]),
                begin,
            )
        print("  [OK] an asset reference picked, undid, cleared and took a drop")

        # Step 3a8c: entity reference rows (W-0048 Stage 6). A field that names
        # another entity draws a row whose text is that entity's Hierarchy
        # label. A scripted pick - the same recorded step the row's picker
        # queues - re-points it and undoes, `none` clears it, a Hierarchy row
        # dragged onto it assigns that entity, and an armed row takes the next
        # scene press as its reference instead of the press selecting. Every
        # way in is one history step, so the row ends where it started.
        reference_dump = session.request(
            "dump entities", re.escape(ENTITIES_DUMP_TOKEN), 30, "the entity list"
        )
        reference_rows = [
            match.group(1)
            for match in re.finditer(
                re.escape(ENTITY_ROW_TOKEN)
                + r'\s+display="([^"]+)"\s+components="([^"]*)"',
                reference_dump,
            )
            if "ShowcaseReferenceComponent" in match.group(2)
        ]
        assert reference_rows, (
            "no entity carries a showcase reference: {}".format(reference_dump[-800:])
        )
        listed_entities = [
            match.group(1)
            for match in re.finditer(
                re.escape(ENTITY_ROW_TOKEN) + r'\s+display="([^"]+)"', reference_dump
            )
        ]
        # A carrier no earlier step selected: a row logs its text on its first
        # draw and on every change, so a freshly selected one always logs.
        holder = next(
            (
                display
                for display in reference_rows
                if display not in (mesh_entity, prior_entity, picked_entity)
            ),
            None,
        )
        assert holder, "every reference carrier was selected by an earlier step"

        def entity_row_capture() -> str:
            """The holder's reference row, capturing the text it names."""
            return (
                re.escape(ENTITY_REFERENCE_ROW_TOKEN)
                + r"(?=[^\n]*entity="
                + re.escape(holder)
                + r"\b)[^\n]*reference=\"([^\"]*)\""
            )

        def entity_row_pattern(reference: str) -> str:
            """The holder's reference row, naming `reference`."""
            return (
                re.escape(ENTITY_REFERENCE_ROW_TOKEN)
                + r"(?=[^\n]*entity="
                + re.escape(holder)
                + r"\b)[^\n]*reference=\""
                + re.escape(reference)
                + r'"'
            )

        begin = len(session.read_log())
        session.send("select entity {}".format(holder))
        session.wait(entity_row_capture(), 30, "the reference row", begin)
        authored_target = last_log_match(session, entity_row_capture(), begin).group(1)
        assert authored_target not in ("None", "(missing)"), (
            "the reference row starts unset: {}".format(authored_target)
        )
        print("  [OK] the reference row names {}".format(authored_target))

        # The picker's step: by label, recorded, and undone back.
        picked_target = next(
            display
            for display in listed_entities
            if display not in (authored_target, holder)
        )
        begin = len(session.read_log())
        session.send(
            "edit set {} ShowcaseReferenceComponent follow {}".format(
                holder, picked_target
            )
        )
        session.wait(
            re.escape(ENTITY_REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="follow")[^\n]*reference="'
            + re.escape(picked_target)
            + r'"',
            60,
            "the reference pick",
            begin,
        )
        session.wait(
            entity_row_pattern(picked_target),
            30,
            "the row names the picked entity",
            begin,
        )
        print("  [OK] a scripted pick pointed the reference at {}".format(picked_target))
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the pick's undo", begin)
        session.wait(
            entity_row_pattern(authored_target),
            30,
            "the authored reference after the undo",
            begin,
        )
        print("  [OK] undoing the pick put the authored reference back")

        # `none` clears the field to the null handle, and undoes back.
        begin = len(session.read_log())
        session.send("edit set {} ShowcaseReferenceComponent follow none".format(holder))
        session.wait(
            re.escape(ENTITY_REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="follow")[^\n]*reference="none"',
            60,
            "the reference cleared",
            begin,
        )
        session.wait(entity_row_pattern("None"), 30, "the row reads unset", begin)
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the clear's undo", begin)
        session.wait(
            entity_row_pattern(authored_target),
            30,
            "the authored reference after the clear's undo",
            begin,
        )
        print("  [OK] clearing the reference undoes back to what it named")

        # The drop: the entities tab's rows are the drag sources, so the tab is
        # opened (the Inspector stays beside it) and both ends' rectangles come
        # from the logs. The rows log their rectangles the first time they are
        # drawn, and the tab body keeps drawing while another tab is in front,
        # so the dragged row's rectangle is already there.
        session.send("tab entities")
        hierarchy_row_pattern = (
            re.escape(HIERARCHY_ROW_RECT_TOKEN)
            + r"\s+entity="
            + re.escape(picked_target)
            + r"\b[^\n]*"
        )
        session.wait(hierarchy_row_pattern, 60, "the hierarchy row's rectangle", 0)
        reference_row_rect_pattern = (
            re.escape(ENTITY_REFERENCE_ROW_RECT_TOKEN)
            + r'(?=[^\n]*row="'
            + re.escape(holder)
            + r"\.)"
            + r"[^\n]*"
        )
        # The reference section sits below others: they are collapsed, topmost
        # first, until its row's rectangle - logged only while the row's centre
        # is visible, which is the box a drop lands in - comes on screen. The
        # row kept drawing since the holder was first selected, so a rectangle
        # it already logged counts.
        collapsed_for_reference = []
        begin = len(session.read_log())
        session.send("select entity {}".format(holder))
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if re.search(reference_row_rect_pattern, session.read_log()):
                break
            headers = {}
            # Only this entity's sections: the shell forgets a previous
            # subject's rectangles, so everything after the last selection
            # change belongs to the entity it shows now.
            log_text = session.read_log()
            last_change = log_text.rfind(SELECTION_CHANGED_TOKEN)
            scoped = log_text[last_change:] if last_change >= 0 else log_text
            for match in re.finditer(INSPECTOR_SECTION_RECT_PATTERN, scoped):
                headers[match.group(1)] = match
            above = sorted(
                (
                    (name, float(logged.group(3)))
                    for name, logged in headers.items()
                    if not name.endswith("::ShowcaseReferenceComponent")
                    and name not in collapsed_for_reference
                ),
                key=lambda entry: entry[1],
            )
            assert above, "every other Inspector section is collapsed and the row stayed off screen"
            component = above[0][0]
            section_begin = len(session.read_log())
            x, y, width, height = inspector_section_rect(session, component)
            session.send("click {:.0f} {:.0f}".format(x + 12.0, y + 6.0))
            session.wait(
                re.escape(INSPECTOR_SECTION_TOKEN)
                + r'(?=[^\n]*component="'
                + re.escape(component)
                + r'")[^\n]*collapsed=true',
                30,
                "the {} section collapsed".format(component.rsplit("::", 1)[-1]),
                section_begin,
            )
            collapsed_for_reference.append(component)
        session.wait(
            reference_row_rect_pattern,
            30,
            "the reference row's rectangle",
            0,
        )
        drop_from = logged_rect_centre(
            last_log_match(session, hierarchy_row_pattern).group(0)
        )
        drop_to = logged_rect_centre(
            last_log_match(session, reference_row_rect_pattern).group(0)
        )
        begin = len(session.read_log())
        session.send(f"drag {drop_from[0]:.0f} {drop_from[1]:.0f} {drop_to[0]:.0f} {drop_to[1]:.0f}")
        session.wait(
            re.escape(ENTITY_REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="follow")[^\n]*reference="'
            + re.escape(picked_target)
            + r'"',
            60,
            "the dropped entity",
            begin,
        )
        session.wait(
            entity_row_pattern(picked_target),
            30,
            "the row names the dropped entity",
            begin,
        )
        print("  [OK] dropping a Hierarchy row onto the reference assigned it")
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the drop's undo", begin)
        session.wait(
            entity_row_pattern(authored_target),
            30,
            "the authored reference after the drop's undo",
            begin,
        )

        # A reference to a destroyed entity reads as missing: a throwaway
        # entity is created, pointed at, deleted and the row re-read, so the
        # scene's own entities are left alone.
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.entity_create")
        session.wait(re.escape(NEW_ENTITY_TOKEN), 30, "the throwaway entity queued", begin)
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="new entity"',
            30,
            "the throwaway recorded",
            begin,
        )
        wait_applied(applied_before, "the throwaway applied")
        # The created entity is the one the list did not have: the selection
        # the create leaves behind is not read, because a later `select entity`
        # in this step would then name the wrong entity.
        after_create = session.request(
            "dump entities", re.escape(ENTITIES_DUMP_TOKEN), 30, "the entity list again"
        )
        created = [
            match.group(1)
            for match in re.finditer(
                re.escape(ENTITY_ROW_TOKEN) + r'\s+display="([^"]+)"', after_create
            )
            if match.group(1) not in listed_entities
        ]
        assert len(created) == 1, "the create added {} entities".format(len(created))
        throwaway = created[0]
        assert throwaway != holder, "the throwaway is the holder itself"
        begin = len(session.read_log())
        session.send(
            "edit set {} ShowcaseReferenceComponent follow {}".format(holder, throwaway)
        )
        session.wait(
            re.escape(ENTITY_REFERENCE_SET_TOKEN)
            + r'(?=[^\n]*field="follow")[^\n]*reference="'
            + re.escape(throwaway)
            + r'"',
            60,
            "the reference pointed at the throwaway",
            begin,
        )
        # The selection has to be on the throwaway before the delete is queued:
        # the delete command reads the selection when it is built.
        begin = len(session.read_log())
        session.send("select entity {}".format(throwaway))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN)
            + r"\s+entity="
            + re.escape(throwaway)
            + r"\b",
            30,
            "the throwaway selected",
            begin,
        )
        begin = len(session.read_log())
        session.send("command editor.entity_delete")
        session.wait(
            re.escape(DELETE_ENTITY_TOKEN) + r"\s+entity=" + re.escape(throwaway) + r"\b",
            30,
            "the throwaway deleted",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="delete entity"',
            30,
            "the delete recorded",
            begin,
        )
        begin = len(session.read_log())
        session.send("select entity {}".format(holder))
        session.wait(
            entity_row_pattern("(missing)"),
            60,
            "the row reads missing",
            begin,
        )
        print("  [OK] a reference to a destroyed entity reads as missing")

        # And back to the entity the scene authored the reference with.
        begin = len(session.read_log())
        session.send(
            "edit set {} ShowcaseReferenceComponent follow {}".format(
                holder, authored_target
            )
        )
        session.wait(
            entity_row_pattern(authored_target),
            60,
            "the authored reference back",
            begin,
        )
        for component in reversed(collapsed_for_reference):
            begin = len(session.read_log())
            x, y, width, height = inspector_section_rect(session, component)
            session.send("click {:.0f} {:.0f}".format(x + 12.0, y + 6.0))
            session.wait(
                re.escape(INSPECTOR_SECTION_TOKEN)
                + r'(?=[^\n]*component="'
                + re.escape(component)
                + r'")[^\n]*collapsed=false',
                30,
                "the {} section expanded".format(component.rsplit("::", 1)[-1]),
                begin,
            )
        heartbeats_before = occurrence_count(session, re.escape(EGUI_ROWS_TOKEN))
        session.send("tab assets")
        wait_for_frames = time.monotonic() + 30
        while time.monotonic() < wait_for_frames:
            if occurrence_count(session, re.escape(EGUI_ROWS_TOKEN)) > heartbeats_before:
                break
            # Poll until a frame drawn after the tab switch reports in.
            time.sleep(0.2)
        print("  [OK] an entity reference was picked, cleared, dropped and went missing")

        begin = len(session.read_log())
        session.send("select entity {}".format(prior_entity))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + r'\s+entity=[^ ]+\s+count=1\b',
            30,
            "the prior selection restored",
            begin,
        )
        print("  [OK] the prior selection is back")

        # Step 3a9: the multi-selection (W-0038 Stage 16). Ctrl adds, Shift
        # ranges, a plain click resets, and a field edit over the set is one
        # recorded step that undoes in one.
        dump = session.request(
            "dump entities", re.escape(ENTITIES_DUMP_TOKEN), 30, "the entity list"
        )
        showcase_entities = [
            match.group(1)
            for match in re.finditer(
                re.escape(ENTITY_ROW_TOKEN)
                + r'\s+display="([^"]+)"\s+components="([^"]*)"',
                dump,
            )
            if SHOWCASE_COMPONENT in match.group(2)
        ]
        assert len(showcase_entities) >= 3, (
            "fewer than three entities carry the showcase component: {}".format(
                showcase_entities
            )
        )
        first, second, third = showcase_entities[:3]
        # What was selected before this step: the viewport steps after it drag
        # the selection's gizmo, so the step puts the selection back.
        prior = session.request(
            "dump selection", re.escape(SELECTION_DUMP_TOKEN), 30, "the prior selection"
        )
        prior_match = re.search(r'primary="([^"]+)"', prior)
        assert prior_match is not None, "the prior selection did not read back: {}".format(
            prior[-400:]
        )
        prior_entity = prior_match.group(1)
        begin = len(session.read_log())
        session.send("select entity {}".format(first))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + r'\s+entity=[^ ]+\s+count=1\b',
            30,
            "the first selection",
            begin,
        )
        begin = len(session.read_log())
        session.send("select add {}".format(second))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + r'\s+entity=[^ ]+\s+count=2\b',
            30,
            "the Ctrl add",
            begin,
        )
        third_add_begin = len(session.read_log())
        session.send("select add {}".format(third))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + r'\s+entity=[^ ]+\s+count=3\b',
            30,
            "the second Ctrl add",
            third_add_begin,
        )
        begin = len(session.read_log())
        session.send("dump selection")
        session.wait(
            re.escape(SELECTION_DUMP_TOKEN) + r'\s+count=3\b',
            30,
            "the selected set",
            begin,
        )
        selection_log = session.read_log()[begin:]
        assert selection_log.count(SELECTED_ENTITY_TOKEN) >= 3, (
            "the dump did not name all three entities: {}".format(selection_log[-600:])
        )
        assert re.search(
            r'primary="{}"'.format(re.escape(third)), selection_log
        ), "the last entity added is not the primary: {}".format(selection_log[-600:])
        print("  [OK] Ctrl extended the selection to three entities")

        # The Inspector marks the rows the selection disagrees on: the
        # showcase component's values differ per model, so the frame after the
        # third entity joined the set draws a mixed row.
        session.wait(
            re.escape(MIXED_ROW_TOKEN), 30, "the mixed inspector row", third_add_begin
        )
        print("  [OK] the Inspector marked a mixed row")

        # One edit over the set: three entities, one recorded step.
        originals = {}
        for entity in (first, second, third):
            log = session.request(
                "dump field {} {} scale".format(entity, SHOWCASE_COMPONENT),
                re.escape(FIELD_TOKEN),
                30,
                "the original scale",
            )
            match = re.search(r"value=(?:F32\()?(-?[0-9.]+)", log)
            assert match is not None, "a scale did not read back: {}".format(log[-400:])
            originals[entity] = float(match.group(1))
        begin = len(session.read_log())
        session.send("edit selection {} scale 2.5".format(SHOWCASE_COMPONENT))
        session.wait(
            re.escape(SELECTION_EDIT_TOKEN)
            + r'\s+component="[^"]*{}"\s+field="scale"\s+entities=3\b'.format(
                re.escape(SHOWCASE_COMPONENT)
            ),
            30,
            "the selection edit",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="set {}\.scale"'.format(re.escape(SHOWCASE_COMPONENT)),
            30,
            "the selection edit recorded one step",
            begin,
        )
        for entity in (first, second, third):
            log = session.request(
                "dump field {} {} scale".format(entity, SHOWCASE_COMPONENT),
                re.escape(FIELD_TOKEN),
                30,
                "the edited scale",
            )
            assert "2.5" in log, (
                "the selection edit missed {}: {}".format(entity, log[-400:])
            )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the selection undo", begin)
        for entity in (first, second, third):
            log = session.request(
                "dump field {} {} scale".format(entity, SHOWCASE_COMPONENT),
                re.escape(FIELD_TOKEN),
                30,
                "the restored scale",
            )
            match = re.search(r"value=(?:F32\()?(-?[0-9.]+)", log)
            assert match is not None and float(match.group(1)) == originals[entity], (
                "one undo did not restore {}: {}".format(entity, log[-400:])
            )
        print("  [OK] a shared-field edit across three entities undid in one step")

        # A plain click resets the set - and puts the selection back where the
        # step found it, so the viewport steps after this one drag the same
        # entity.
        begin = len(session.read_log())
        session.send("select entity {}".format(prior_entity))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + r'\s+entity=[^ ]+\s+count=1\b',
            30,
            "the resetting click",
            begin,
        )
        print("  [OK] a plain click reset the selection")

        # Step 3a10: the clipboard (W-0038 Stage 17). A copy leaves the
        # editor under its registered format, which another process reads back
        # here; a component payload written from outside pastes onto the
        # selection in one undo step; and a second editor instance pastes the
        # entity copy, which is what "across editor instances" means.
        win_clipboard.clear()
        begin = len(session.read_log())
        session.send("command editor.copy")
        session.wait(
            re.escape(CLIPBOARD_WRITTEN_TOKEN), 30, "the clipboard write", begin
        )
        payload = json.loads(win_clipboard.read_payload())
        assert payload.get("kind") == "entity", (
            "the registered format does not carry an entity payload: {}".format(payload)
        )
        assert payload.get("pill") == 1, "the payload's version is not 1"
        print("  [OK] the copy reached the OS clipboard as an entity payload")

        component_payload = {
            "pill": 1,
            "kind": "component",
            "component": {"type": SHOWCASE_TYPE_NAME, "fields": [["frame", "7.25f"]]},
        }
        win_clipboard.write_payload(json.dumps(component_payload))
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.paste")
        session.wait(
            re.escape(CLIPBOARD_PASTED_TOKEN) + r'\s+kind="component"',
            30,
            "the component paste",
            begin,
        )
        # The paste is logged when the payload is decoded; its commands cross
        # the frame boundary after that, so the dump below would otherwise race
        # the write it is meant to read.
        wait_applied(applied_before, "the component paste applied")
        begin = len(session.read_log())
        session.send(
            "dump field {} {} frame".format(prior_entity, SHOWCASE_COMPONENT)
        )
        session.wait(re.escape(FIELD_TOKEN), 30, "the pasted field", begin)
        assert "7.25" in session.read_log()[begin:], (
            "the component paste did not apply: {}".format(
                session.read_log()[begin:][-600:]
            )
        )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the paste's undo", begin)
        begin = len(session.read_log())
        session.send(
            "dump field {} {} frame".format(prior_entity, SHOWCASE_COMPONENT)
        )
        session.wait(re.escape(FIELD_TOKEN), 30, "the restored pasted field", begin)
        assert "7.25" not in session.read_log()[begin:], (
            "one undo did not take the whole component paste back: {}".format(
                session.read_log()[begin:][-600:]
            )
        )
        print("  [OK] a component paste applied and undid in one step")

        # Step 3a11: an OS drop into Assets (W-0038 Stage 18). winit's drop
        # events cannot be synthesised from the command file, so the paths go to
        # the same handler they call: the preview names the folder, the file is
        # copied into it as one recorded step, and undo takes it off the disk.
        # A folder is refused by name instead of being copied.
        drop_source = REPOSITORY_ROOT / "local" / "temp" / "s18_dropped.png"
        drop_source.parent.mkdir(parents=True, exist_ok=True)
        drop_source.write_bytes(b"\x89PNG\r\n\x1a\ndrop probe\n")
        begin = len(session.read_log())
        session.send("hover file {}".format(drop_source))
        session.wait(
            re.escape(DROP_PREVIEW_TOKEN), 30, "the drop preview", begin
        )
        begin = len(session.read_log())
        session.send("drop {}".format(drop_source))
        session.wait(
            re.escape(FILES_DROPPED_TOKEN) + r"\s+count=1\b",
            30,
            "the dropped file",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="import files"',
            30,
            "the import step",
            begin,
        )
        # The import is a file copy the editor does on its own thread; poll
        # the tree rather than assume the copy finished with the log line.
        time.sleep(0.5)
        imported = list(asset_root.rglob("s18_dropped.png"))
        assert len(imported) == 1, (
            "the dropped file was not copied into the project: {}".format(imported)
        )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(re.escape(HISTORY_UNDO_TOKEN), 30, "the import's undo", begin)
        # The undo deletes the copied file on its own turn; give the removal
        # the moment it needs before the assertion below reads the tree.
        time.sleep(0.5)
        assert not imported[0].exists(), (
            "the undo left the imported file on disk: {}".format(imported[0])
        )
        print("  [OK] a dropped file was imported and undone")

        begin = len(session.read_log())
        session.send("drop {}".format(drop_source.parent))
        session.wait(
            re.escape(DROP_REFUSED_FOLDER_TOKEN), 30, "the folder refusal", begin
        )
        session.wait(
            re.escape(TOAST_TOKEN) + r'.*is a folder',
            30,
            "the refusal toast",
            begin,
        )
        print("  [OK] a dropped folder is refused with a toast")

        # A drop folder the scripted channel climbed out of (W-0107): the join
        # goes through the same validator every other asset command uses, so
        # `res/../..` is refused instead of being joined onto the asset root -
        # the copy would land in `examples/`, outside the project. Both lines go
        # in one write: the channel polls once per frame, so the drop runs while
        # the browser still holds the climbed-out folder (the listing settles it
        # back to the root between frames).
        begin = len(session.read_log())
        session.send_many(["browser ../..", "drop {}".format(drop_source)])
        session.wait(
            re.escape(DROP_REFUSED_PATH_TOKEN)
            + r".*is not a path inside the asset directory",
            30,
            "the traversal refusal",
            begin,
        )
        # A queued import would copy on a later frame, so the assertions below
        # are only worth anything if that frame had time to run.
        time.sleep(0.5)
        escaped = asset_root.parent.parent / drop_source.name
        assert not escaped.exists(), (
            "a drop carried the file outside the asset directory: {}".format(escaped)
        )
        assert not list(asset_root.rglob(drop_source.name)), (
            "the refused drop still copied the file into the project"
        )
        print("  [OK] a drop folder outside the asset directory is refused")
        session.send("browser root")

        # Step 3a12: the drag-out smoke test (W-0038 Stage 19). Dropping into
        # Explorer needs a person, so what this checks is everything up to
        # OLE: the asset's path resolves to a file and becomes a data object.
        # The drag then ends without entering OLE's loop, because no button is
        # held - the same rule the drop source applies to a released button.
        drag_source = REPOSITORY_ROOT / "local" / "temp" / "s19_drag.png"
        drag_source.write_bytes(b"\x89PNG\r\n\x1a\ndrag probe\n")
        begin = len(session.read_log())
        session.send("drag out {}".format(drag_source))
        session.wait(
            re.escape(DRAG_OUT_PREPARED_TOKEN), 60, "the drag-out hand-off", begin
        )
        session.wait(
            re.escape(DRAG_OUT_NO_BUTTON_TOKEN), 60, "the drag-out refusal", begin
        )
        print("  [OK] an asset reached OLE's data object, and the drag ends safely")

        # Park the tool on rotate: the next step's `gizmo translate` only logs
        # a change, and the bound chord has just taken the tool to translate.
        begin = len(session.read_log())
        session.send("gizmo rotate")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r'\s+mode="rotate"',
            30,
            "the tool parked on rotate for the next step",
            begin,
        )

        # Step 3b: the viewport's interaction states (Stage 28). A drag begins
        # on the press and ends on the release; the hover highlight follows
        # the pointer; the axis guide and the displacement line appear with
        # the drag; and the translation tool's plane handles move in their
        # own plane.
        begin = len(session.read_log())
        session.send("gizmo translate")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r'\s+mode="translate"',
            30,
            "the translate tool is active again",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump viewport")
        session.wait(
            re.escape(VIEWPORT_DUMP_TOKEN) + r'\s+active=0\s+interaction="idle"',
            30,
            "the viewport is idle",
            begin,
        )
        print("  [OK] the viewport starts active and idle")

        # The press reports the exact handle pixel and starts the drag - no
        # release needed - and a release with no move finishes it.
        press_begin = len(session.read_log())
        session.send("gizmo press x")
        press_pattern = (
            re.escape(GIZMO_DRAG_PRESS_TOKEN) + r'\s+axis="x"\s+x=([-\d.]+)\s+y=([-\d.]+)'
        )
        session.wait(press_pattern, 30, "the X handle was pressed", press_begin)
        press = session.find(press_pattern, press_begin)
        press_x, press_y = float(press.group(1)), float(press.group(2))
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN) + r'\s+mode="translate"\s+axis="x"',
            60,
            "the drag started on the press",
            press_begin,
        )
        print("  [OK] the drag begins on the press, before any release")
        session.wait(
            re.escape(GIZMO_GUIDES_TOKEN) + r'\s+axis="x"\s+dotted=true',
            30,
            "the axis guide and the displacement line appeared",
            press_begin,
        )
        begin = len(session.read_log())
        session.send("dump viewport")
        session.wait(
            re.escape(VIEWPORT_DUMP_TOKEN) + r'\s+active=0\s+interaction="dragging-axis"',
            30,
            "the drag owns the viewport",
            begin,
        )
        begin = len(session.read_log())
        session.send("gizmo release")
        session.wait(
            re.escape(GIZMO_DRAG_RELEASED_TOKEN) + r'\s+mode="translate"\s+axis="x"',
            60,
            "the release ended the drag",
            begin,
        )
        session.wait(
            re.escape(GIZMO_GUIDES_TOKEN) + r'\s+guides=false',
            30,
            "the guides cleared with the drag",
            begin,
        )
        print("  [OK] the release ends the drag and its guides")

        # The pointer over the handle highlights it, and leaving the gizmo
        # clears the highlight. The press moved nothing, so the handle still
        # sits on the pixel the press reported.
        begin = len(session.read_log())
        session.send(f"hover 0 {press_x:.0f} {press_y:.0f}")
        session.wait(
            re.escape(GIZMO_HOVER_TOKEN) + r'\s+view=0\s+handle=1',
            30,
            "the X handle highlighted",
            begin,
        )
        begin = len(session.read_log())
        session.send("hover 0 5 5")
        session.wait(
            re.escape(GIZMO_HOVER_NONE_TOKEN),
            30,
            "the highlight cleared",
            begin,
        )
        print("  [OK] the pointer highlights the handle under it")

        # A plane handle drags in its own plane: X and Y follow the drag, Z
        # keeps its value.
        before_plane = field_values(gizmo_entity, "translation")
        begin = len(session.read_log())
        session.send("gizmo drag xy 40 30")
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN) + r'\s+mode="translate"\s+plane="xy"',
            60,
            "the drag started on the XY square",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN) + r'\s+mode="translate"\s+plane="xy"',
            60,
            "the plane drag ended",
            begin,
        )
        after_plane = field_values(gizmo_entity, "translation")
        assert abs(after_plane[0] - before_plane[0]) > 0.05 and abs(
            after_plane[1] - before_plane[1]
        ) > 0.05, f"the plane drag did not move both axes: {before_plane} -> {after_plane}"
        assert abs(after_plane[2] - before_plane[2]) < 1e-4, (
            f"the plane drag left the plane on z: {before_plane} -> {after_plane}"
        )
        print(f"  [OK] the XY plane drag moved {before_plane} -> {after_plane}")

        # Step 4: the Assets pipeline - the listing, a create through its
        # dialog, a rename whose move the selection follows, a move into a
        # folder, a delete that undoes back, and the write-meta action. The
        # dialogs are driven through the scripted channel (the toolbar and
        # dialog commands take the same paths its widgets do) and the world
        # through the log tokens and the disk.

        def listing_count() -> int:
            """The newest asset-listing count the frontend logged."""
            counts = [
                int(value)
                for value in re.findall(
                    re.escape(ASSET_LISTING_TOKEN) + r"\s+count=(\d+)", session.read_log()
                )
            ]
            return counts[-1] if counts else 0

        def wait_for_path(path: Path, exists: bool, timeout: float, label: str) -> None:
            """Polls the disk until the path's presence matches."""
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if path.exists() == exists:
                    print(f"  [OK] {label}")
                    return
                # Poll until the command drain applies and the disk catches up.
                time.sleep(0.2)
            raise TimeoutError(
                f"{label}: {path} still {'absent' if exists else 'present'}\n"
                + session.read_log()[-2000:]
            )

        session.wait(
            re.escape(ASSET_LISTING_TOKEN) + r"\s+count=(\d+)\s+types=(\d+)",
            120,
            "asset listing refreshed",
        )
        match = re.search(
            re.escape(ASSET_LISTING_TOKEN) + r"\s+count=(\d+)\s+types=(\d+)",
            session.read_log(),
        )
        listed, typed = int(match.group(1)), int(match.group(2))
        assert listed >= 1, "the asset listing is empty"
        assert typed >= 1, "no standalone asset types are loaded; the create dialog cannot act"
        print(f"  [OK] asset listing {listed} entries, {typed} standalone types")

        begin = len(session.read_log())
        session.send("dump assets")
        session.wait(
            re.escape(ASSETS_DUMP_TOKEN) + r"\s+count=\d+", 30, "assets dumped", begin
        )
        dump = session.read_log()[begin:]
        assert re.search(r'path="?textures/?"?\s+kind="?folder', dump), (
            "the listing misses the textures folder: " + dump[-800:]
        )
        staged_row = re.search(
            r'path="?s16_stage\.png"?\s+kind="?(\S+)"?\s+standalone=(\S+)\s+has_meta=(\S+)',
            dump,
        )
        assert staged_row, "the staged meta probe is not in the listing: " + dump[-800:]
        assert staged_row.group(3) == "false", "the staging probe already has a sidecar"

        # The browser is Unity's project panel: a folder tree beside a grid of
        # the open folder's items. A scripted navigation lands the grid on
        # textures, which lists that folder's own entries and nothing that
        # lives at the root.
        begin = len(session.read_log())
        session.send("browser textures")
        session.wait(
            re.escape(BROWSER_FOLDER_TOKEN) + r'\s+path="(textures)"',
            30,
            "the browser opened the textures folder",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump browser")
        session.wait(
            re.escape(BROWSER_DUMP_TOKEN) + r'\s+folder="textures"\s+count=(\d+)',
            30,
            "browser dumped",
            begin,
        )
        grid = session.read_log()[begin:]
        assert re.search(
            re.escape(BROWSER_ITEM_TOKEN)
            + r'\s+path="?textures/chimpanzini_bananini_color\.png',
            grid,
        ), "the grid misses the textures folder's texture: " + grid[-800:]
        for outside in ("config.ini", "s16_stage.png"):
            assert not re.search(
                re.escape(BROWSER_ITEM_TOKEN) + r'\s+path="?' + re.escape(outside), grid
            ), "the grid lists an entry outside the open folder: " + grid[-800:]
        print("  [OK] the browser's grid lists the open folder")
        # Back to the asset root, the tree's first row, for the steps below.
        begin = len(session.read_log())
        session.send("browser root")
        session.wait(
            re.escape(BROWSER_FOLDER_TOKEN) + r'\s+path=""',
            30,
            "the browser returned to the asset root",
            begin,
        )

        # A module's resources are listed beside the project's and can never be
        # written. The listing marks the root and the rule; the tree opens that
        # root; the Inspector says its section is read-only; and a delete is
        # refused with a warning instead of being queued, so no history step
        # appears to undo.
        module_rows = [
            match.groups()
            for match in re.finditer(
                r'path="?(extensions/[^\s"]+)"?\s+kind="?([^\s"]+)"?\s+standalone=\S+'
                r'\s+has_meta=\S+\s+root="?([^\s"]+)"?\s+read_only=(true|false)',
                dump,
            )
        ]
        module_files = [
            row for row in module_rows if row[1] != "folder" and row[3] == "true" and row[2]
        ]
        assert module_files, "no module resource is listed read-only: " + dump[-800:]
        module_key, _, module_root, _ = module_files[0]
        module_folder = module_key.rsplit("/", 1)[0]
        # The browser's grid names a row by its path inside its root, so the
        # root's own prefix comes off before it is matched.
        module_relative = module_key.split(f"extensions/{module_root}/", 1)[-1]
        print(f"  [OK] {module_root} lists {module_key} read-only")

        begin = len(session.read_log())
        session.send(f"browser {module_folder}")
        session.wait(
            re.escape(BROWSER_FOLDER_TOKEN)
            + r'\s+path="'
            + re.escape(module_folder)
            + r'"',
            30,
            "the module's folder opened",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump browser")
        session.wait(re.escape(BROWSER_DUMP_TOKEN), 30, "the module folder dumped", begin)
        module_grid = session.read_log()[begin:]
        assert re.search(
            re.escape(BROWSER_ITEM_TOKEN) + r'\s+path="?' + re.escape(module_relative), module_grid
        ), "the grid misses the module's own file: " + module_grid[-800:]

        # Select the module's file: the Inspector draws its section read-only,
        # which is the one line the tab prints for it.
        begin = len(session.read_log())
        session.send(f"select asset {module_key}")
        session.wait(
            re.escape(ASSET_SELECTED_TOKEN) + r'\s+path="' + re.escape(module_key) + r'"',
            30,
            "the module's file selected",
            begin,
        )
        # The Inspector draws the section, so it has to be the open tab for
        # the row line to appear at all.
        session.send("tab inspector")
        session.wait(
            re.escape(ASSET_READ_ONLY_ROW_TOKEN) + r'\s+asset="?' + re.escape(module_key) + r'"?',
            30,
            "the Inspector drew the module asset read-only",
            begin,
        )

        # The delete is refused where the toolbar's enablement already says it
        # would be: no queued delete, no history step, and the file is still
        # listed afterwards.
        begin = len(session.read_log())
        session.send("toolbar delete")
        session.wait(
            re.escape(ASSET_DELETE_REFUSED_TOKEN) + r'\s+path="' + re.escape(module_key) + r'"',
            30,
            "the module asset's delete refused",
            begin,
        )
        refused_window = session.read_log()[begin:]
        assert ASSET_DELETE_QUEUED_TOKEN not in refused_window, (
            "a module asset's delete was queued: " + refused_window[-800:]
        )
        assert HISTORY_PUSH_TOKEN not in refused_window, (
            "the refused delete pushed a history step: " + refused_window[-800:]
        )
        begin = len(session.read_log())
        session.send("dump assets")
        session.wait(re.escape(ASSETS_DUMP_TOKEN), 30, "the listing re-dumped", begin)
        assert re.search(
            r'path="?' + re.escape(module_key), session.read_log()[begin:]
        ), "the refused delete removed the module asset from the listing"
        print("  [OK] a module asset's delete was refused with no history step")

        # Back to the Assets tab and the project's root for the steps below.
        session.send("tab assets")
        begin = len(session.read_log())
        session.send("browser root")
        session.wait(
            re.escape(BROWSER_FOLDER_TOKEN) + r'\s+path=""',
            30,
            "the browser returned to the asset root again",
            begin,
        )

        begin = len(session.read_log())
        session.send("dump standalone-types")
        session.wait(re.escape(STANDALONE_TYPE_TOKEN), 30, "standalone types dumped", begin)
        found = session.find(
            re.escape(STANDALONE_TYPE_TOKEN)
            + r'\s+type_name="([^"]+)"\s+display="([^"]+)"\s+extension="([^"]+)"',
            begin,
        )
        extension = found.group(3)
        created = f"textures/s16_probe.{extension}"
        created_path = asset_root / "textures" / f"s16_probe.{extension}"
        print(f"  [OK] first standalone type is .{extension}")

        # Create through the dialog: select the textures folder (the dialog
        # seeds it), open the create dialog from the toolbar, name the file
        # and submit; the queued command is recorded and walks back through
        # undo.
        begin = len(session.read_log())
        session.send("select asset textures")
        session.wait(
            re.escape(ASSET_SELECTED_TOKEN) + r'\s+path="(textures)"',
            30,
            "a folder row selected",
            begin,
        )
        begin = len(session.read_log())
        session.send("toolbar create")
        session.wait(
            re.escape(ASSET_DIALOG_OPENED_TOKEN) + r'\s+dialog="create"',
            30,
            "create dialog opened",
            begin,
        )
        session.send("dialog set name s16_probe")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("dialog submit")
        session.wait(re.escape(ASSET_CREATE_QUEUED_TOKEN), 30, "create queued", begin)
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="create asset s16_probe"',
            30,
            "create recorded",
            begin,
        )
        session.wait(
            re.escape(ASSET_DIALOG_CLOSED_TOKEN) + r'\s+dialog="create"',
            30,
            "create dialog closed",
            begin,
        )
        wait_applied(applied_before, "create applied")
        wait_for_path(created_path, True, 30, f"created {created}")
        before = listed
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and listing_count() < before + 1:
            # Poll until the periodic walk sees the new file.
            time.sleep(0.5)
        assert listing_count() >= before + 1, "the listing did not pick the created file up"

        # The creation undoes (the file goes) and redoes (it comes back).
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="create asset s16_probe"',
            30,
            "create undone",
            begin,
        )
        wait_applied(applied_before, "undo applied")
        wait_for_path(created_path, False, 30, "the creation's undo removed the file")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.redo")
        session.wait(
            re.escape(HISTORY_REDO_TOKEN) + r'\s+label="create asset s16_probe"',
            30,
            "create redone",
            begin,
        )
        wait_applied(applied_before, "redo applied")
        wait_for_path(created_path, True, 30, "the redo restored the file")

        # Rename through the dialog: the field arrives with the stem in it,
        # the typed name keeps the extension, and the selection follows the
        # move.
        renamed = f"textures/s16_renamed.{extension}"
        renamed_path = asset_root / "textures" / f"s16_renamed.{extension}"
        begin = len(session.read_log())
        session.send(f"select asset {created}")
        session.wait(
            re.escape(ASSET_SELECTED_TOKEN)
            + r'\s+path="'
            + re.escape(created)
            + r'"',
            30,
            "the created row selected",
            begin,
        )
        begin = len(session.read_log())
        session.send("toolbar rename")
        session.wait(
            re.escape(ASSET_DIALOG_OPENED_TOKEN) + r'\s+dialog="rename"',
            30,
            "rename dialog opened",
            begin,
        )
        session.send("dialog set name s16_renamed")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("dialog submit")
        session.wait(
            re.escape(ASSET_RENAME_QUEUED_TOKEN)
            + r'\s+from="'
            + re.escape(created)
            + r'"\s+to="'
            + re.escape(renamed)
            + r'"',
            30,
            "rename queued",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="rename asset s16_renamed\.'
            + re.escape(extension)
            + r'"',
            30,
            "rename recorded",
            begin,
        )
        session.wait(
            re.escape(SELECTION_FOLLOWED_TOKEN) + r'\s+path="' + re.escape(renamed) + r'"',
            30,
            "the selection followed the rename",
            begin,
        )
        wait_applied(applied_before, "rename applied")
        wait_for_path(renamed_path, True, 30, f"renamed to {renamed}")
        wait_for_path(created_path, False, 30, "the old path is gone")

        # A cross-folder move (the scripted direct edit; no drag gesture yet)
        # carries the selection the same way.
        moved = f"models/s16_moved.{extension}"
        moved_path = asset_root / "models" / f"s16_moved.{extension}"
        begin = len(session.read_log())
        session.send(f"edit asset move {renamed} {moved}")
        session.wait(
            re.escape(SELECTION_FOLLOWED_TOKEN) + r'\s+path="' + re.escape(moved) + r'"',
            60,
            "the selection followed the move",
            begin,
        )
        wait_for_path(moved_path, True, 30, f"moved to {moved}")

        # A refused move: the destination is an existing folder, so nothing
        # moves and the selection stays where the file is. The follow reads
        # what applied rather than what was asked for, or the Inspector would
        # be pointed at a path the move never made.
        begin = len(session.read_log())
        session.send(f"edit asset move {moved} models")
        session.wait(
            re.escape(COMMAND_FAILED_TOKEN) + r'(?=[^\n]*already exists)',
            30,
            "the move onto an existing path was refused",
            begin,
        )
        refused_window = session.read_log()[begin:]
        assert SELECTION_FOLLOWED_TOKEN not in refused_window, (
            "the selection followed a move that never happened: "
            + refused_window[-800:]
        )
        wait_for_path(moved_path, True, 30, "the refused move left the file in place")
        print("  [OK] a refused move left the selection and the file alone")

        # The delete clears the selection and undoes back into the file.
        begin = len(session.read_log())
        session.send("toolbar delete")
        session.wait(
            re.escape(ASSET_DELETE_QUEUED_TOKEN) + r'\s+path="' + re.escape(moved) + r'"',
            30,
            "delete queued",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="delete asset '
            + re.escape(moved)
            + r'"',
            30,
            "delete recorded",
            begin,
        )
        session.wait(
            re.escape(SELECTION_CLEARED_TOKEN) + r'\s+path="' + re.escape(moved) + r'"',
            30,
            "the selection cleared with the delete",
            begin,
        )
        wait_for_path(moved_path, False, 30, "deleted from disk")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="delete asset ' + re.escape(moved) + r'"',
            30,
            "delete undone",
            begin,
        )
        wait_applied(applied_before, "delete undo applied")
        wait_for_path(moved_path, True, 30, "the delete's undo restored the file")

        # The write-meta action on the staged sourced file: the sidecar is
        # written action by action and the undo removes it again.
        stage_meta = asset_root / "s16_stage.png.meta"
        begin = len(session.read_log())
        session.send("select asset s16_stage.png")
        session.wait(
            re.escape(ASSET_SELECTED_TOKEN) + r'\s+path="(s16_stage\.png)"',
            30,
            "the staged row selected",
            begin,
        )
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("toolbar meta")
        session.wait(
            re.escape(ASSET_META_QUEUED_TOKEN) + r'\s+path="(s16_stage\.png)"',
            30,
            "meta queued",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="write meta s16_stage\.png"',
            30,
            "meta recorded",
            begin,
        )
        wait_applied(applied_before, "meta applied")
        wait_for_path(stage_meta, True, 30, "the sidecar was written")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="write meta s16_stage\.png"',
            30,
            "meta write undone",
            begin,
        )
        wait_applied(applied_before, "meta undo applied")
        wait_for_path(stage_meta, False, 30, "the sidecar's undo removed it")

        # The Inspector's asset rows are editable too (Stage 17): the
        # selected asset's import settings round-trip through the same
        # queue - a scripted `edit asset-settings` mirrors a widget edit,
        # the `.meta` sidecar on disk is the proof, and the undo restores
        # the original value. The project's cube has a boolean row
        # (`flip_v`) and its color texture has an enum one
        # (`texture_type`), so both widget shapes are covered.
        def wait_meta_value(path: Path, key: str, expected, timeout: float, label: str) -> None:
            """Polls a `.meta` sidecar until `settings.<key>` carries `expected`."""
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    document = json.loads(path.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    document = {}
                if document.get("settings", {}).get(key) == expected:
                    print(f"  [OK] {label}")
                    return
                # Poll until the apply writes the sidecar.
                time.sleep(0.2)
            raise TimeoutError(
                f"{label}: {path} settings.{key} is not {expected!r}\n"
                + session.read_log()[-2000:]
            )

        cube_asset = "models/cube.obj"
        cube_meta = asset_root / "models" / "cube.obj.meta"
        begin = len(session.read_log())
        session.send(f"select asset {cube_asset}")
        session.wait(
            re.escape(ASSET_SELECTED_TOKEN) + r'\s+path="' + re.escape(cube_asset) + r'"',
            30,
            "the cube row selected",
            begin,
        )
        flip_before = json.loads(cube_meta.read_text(encoding="utf-8"))["settings"]["flip_v"]
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(
            f"edit asset-settings {cube_asset} flip_v {str(not flip_before).lower()}"
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="set asset flip_v"',
            30,
            "the boolean row's edit recorded",
            begin,
        )
        wait_applied(applied_before, "settings edit applied")
        wait_meta_value(cube_meta, "flip_v", not flip_before, 30, "the sidecar carries the edit")
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="set asset flip_v"',
            30,
            "the boolean row's edit undid",
            begin,
        )
        wait_applied(applied_before, "settings undo applied")
        wait_meta_value(cube_meta, "flip_v", flip_before, 30, "the undo restored the sidecar")

        texture_asset = "textures/chimpanzini_bananini_color.png"
        texture_meta = asset_root / "textures" / "chimpanzini_bananini_color.png.meta"
        begin = len(session.read_log())
        session.send(f"select asset {texture_asset}")
        session.wait(
            re.escape(ASSET_SELECTED_TOKEN) + r'\s+path="' + re.escape(texture_asset) + r'"',
            30,
            "the color texture row selected",
            begin,
        )
        texture_before = json.loads(texture_meta.read_text(encoding="utf-8"))["settings"][
            "texture_type"
        ]
        texture_next = "Normal" if texture_before == "Color" else "Color"
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(
            f"edit asset-settings {texture_asset} texture_type {texture_next}"
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="set asset texture_type"',
            30,
            "the enum row's edit recorded",
            begin,
        )
        wait_applied(applied_before, "enum edit applied")
        wait_meta_value(
            texture_meta, "texture_type", texture_next, 30, "the enum landed in the sidecar"
        )
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="set asset texture_type"',
            30,
            "the enum row's edit undid",
            begin,
        )
        wait_applied(applied_before, "enum undo applied")
        wait_meta_value(
            texture_meta, "texture_type", texture_before, 30, "the undo restored the enum"
        )
        print("  [OK] inspector asset settings: boolean and enum rows round-trip")
        print("  [OK] assets pipeline: create, rename, move, delete and meta round-trip")

        # Step 4b: the Systems tab's data and controls - the dump lists the
        # registered systems with the state the tab draws, a scripted toggle
        # is a recorded undo step, and the pause and step commands reach the
        # engine (the rows heartbeat carries the pause state the tab's
        # button reads).

        def dump_systems_state() -> "tuple[dict[int, bool], bool]":
            """One `dump systems` round-trip: index -> enabled, and the pause flag."""
            seen = occurrence_count(session, re.escape(SYSTEMS_DUMP_TOKEN))
            begin = len(session.read_log())
            session.send("dump systems")
            dumped = session.wait_occurrence(
                re.escape(SYSTEMS_DUMP_TOKEN) + r"\s+count=(\d+)\s+paused=(true|false)",
                seen + 1,
                60,
                "systems dump",
            )
            dump = session.read_log()[begin:]
            states = {
                int(match.group(1)): match.group(4) == "true"
                for match in re.finditer(SYSTEM_ROW_PATTERN, dump)
            }
            return states, "paused=true" in dumped

        states, paused = dump_systems_state()
        assert states, "the systems dump carried no rows"
        assert all(states.values()), f"a system started disabled: {states}"
        assert not paused, "the engine started paused"
        first_index = min(states)
        print(f"  [OK] systems dumped: {len(states)} rows, index {first_index} first")

        # One toggle off is a recorded edit; the dump is the tab's data, so
        # seeing it flip proves the tab draws the new state. The undo puts
        # it back.
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(f"edit system {first_index} off")
        session.wait(
            re.escape(SYSTEM_EDIT_QUEUED_TOKEN)
            + rf"\s+index={first_index}\s+enabled=false",
            30,
            "the system toggle queued",
            begin,
        )
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + rf'\s+label="system {first_index} off"',
            30,
            "the system toggle recorded",
            begin,
        )
        wait_applied(applied_before, "system toggle applied")
        states, _ = dump_systems_state()
        assert states.get(first_index) is False, "the system toggle did not reach the world"
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + rf'\s+label="system {first_index} off"',
            30,
            "the system toggle undid",
            begin,
        )
        wait_applied(applied_before, "system undo applied")
        states, _ = dump_systems_state()
        assert states.get(first_index) is True, "the undo did not restore the system"

        # Pause, step and resume: the commands the tab's header row queues;
        # the rows heartbeat reports the pause flag the button's label
        # reads.
        begin = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(PAUSE_TOGGLED_TOKEN) + r"\s+paused=true",
            30,
            "the pause toggle",
            begin,
        )
        session.wait(
            re.escape(EGUI_ROWS_TOKEN) + r"\s+frames=\d+[^\n]*paused=true",
            60,
            "the rows heartbeat saw the pause",
            begin,
        )
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.step")
        session.wait(re.escape(STEP_QUEUED_TOKEN), 30, "the step command", begin)
        wait_applied(applied_before, "step applied")
        begin = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(PAUSE_TOGGLED_TOKEN) + r"\s+paused=false",
            30,
            "the resume toggle",
            begin,
        )
        session.wait(
            re.escape(EGUI_ROWS_TOKEN) + r"\s+frames=\d+[^\n]*paused=false",
            60,
            "the rows heartbeat saw the resume",
            begin,
        )
        print("  [OK] systems tab: dump, recorded toggle with undo, pause and step")

        # Step 4c: the context menus. The viewport's menu opens on a real
        # right-click; the menu actions are the same queued commands the
        # scripted entries drive here - the entity create/delete pair (one
        # recorded step each; the delete's undo restores the entity AND the
        # selection that pointed at it), a component detach and attach, and
        # the gizmo tool, snapping and space switches the viewport menu's
        # items queue. The other areas' menus (entities, Inspector, Assets,
        # dock tabs) are probe-captured.

        def latest_entity_count() -> int:
            """The newest rows heartbeat's entity count."""
            counts = [int(match.group(2)) for match in re.finditer(ROWS_PATTERN, session.read_log())]
            return counts[-1] if counts else 0

        def wait_for_entity_count(target: int, label: str) -> None:
            """Waits for a rows heartbeat past the current ones reporting `target`."""
            highest = max(rows_frames(session), default=0)
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline:
                for frame, (entities, _) in rows_frames(session).items():
                    if frame > highest and entities == target:
                        print(f"  [OK] {label}")
                        return
                # Poll until a heartbeat newer than the line drawn before this
                # step reports the target count.
                time.sleep(0.3)
            raise TimeoutError(
                f"{label}: no heartbeat past frame {highest} reported "
                f"entities={target}\n" + session.read_log()[-2000:]
            )

        # Re-select an entity (step 4 left an asset selected): the scripted
        # `select entity` walks the same recorded path the Entities list's
        # row click queues - a screen-space grid cannot hit the entity the
        # gizmo drags moved.
        menu_entity = gizmo_entity
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(f"select entity {menu_entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(menu_entity)}",
            30,
            "the entity re-selected for the menu step",
            begin,
        )
        wait_applied(applied_before, "the selection command applied")
        session.wait(
            re.escape(EGUI_ROWS_TOKEN) + rf"[^\n]*detail=\"{re.escape(menu_entity)}\"",
            60,
            "the re-picked entity reached the Inspector detail",
            begin,
        )
        match = re.search(
            re.escape(EGUI_ROWS_TOKEN)
            + rf"[^\n]*detail=\"{re.escape(menu_entity)}\"\s+detail_components=(\d+)",
            session.read_log()[begin:],
        )
        components_before = int(match.group(1))
        assert components_before >= 1, "the re-picked entity carries no components to detach"

        # The viewport's menu opens on a real right-click over the scene:
        # the bottom-left of the viewport, clear of both the overlay chips
        # (top-right) and the floating toolbox (top-left), which consumes
        # whatever lands on it.
        begin = len(session.read_log())
        session.send(f"rclick {scene_x + 60} {scene_y + scene_height - 60}")
        session.wait(
            re.escape(CONTEXT_MENU_TOKEN) + r'\s+area="scene"',
            30,
            "the viewport menu opened",
            begin,
        )

        # The tool switches: the anchor line carries the state the menu's
        # items set, so each switch is asserted through it.
        begin = len(session.read_log())
        session.send("command editor.tool_rotate")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN)
            + rf'\s+entity={re.escape(menu_entity)}\s+mode="rotate"',
            60,
            "the tool switched to rotate",
            begin,
        )
        begin = len(session.read_log())
        session.send("command editor.tool_snap_toggle")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r"[^\n]*snapped=true",
            60,
            "snapping toggled on",
            begin,
        )
        begin = len(session.read_log())
        session.send("command editor.tool_snap_toggle")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r"[^\n]*snapped=false",
            60,
            "snapping toggled off",
            begin,
        )
        begin = len(session.read_log())
        session.send("command editor.tool_space_toggle")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r'[^\n]*space="local"',
            60,
            "the space switched to local",
            begin,
        )
        begin = len(session.read_log())
        session.send("command editor.tool_space_toggle")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r'[^\n]*space="world"',
            60,
            "the space switched back",
            begin,
        )
        print("  [OK] the viewport menu's tool switches reach the gizmo")

        # The component detach and attach, recorded under the native labels:
        # the heartbeat's detail_components is the count the Inspector draws.
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(f"edit remove {menu_entity} {TRANSFORM_COMPONENT}")
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="remove TransformComponent"',
            30,
            "the component detach recorded",
            begin,
        )
        wait_applied(applied_before, "component detach applied")
        session.wait(
            re.escape(EGUI_ROWS_TOKEN)
            + rf"[^\n]*detail=\"{re.escape(menu_entity)}\""
            + rf"\s+detail_components={components_before - 1}",
            60,
            "the component count dropped",
            begin,
        )
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send(f"edit add {menu_entity} {TRANSFORM_COMPONENT}")
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="add TransformComponent"',
            30,
            "the component attach recorded",
            begin,
        )
        wait_applied(applied_before, "component attach applied")
        session.wait(
            re.escape(EGUI_ROWS_TOKEN)
            + rf"[^\n]*detail=\"{re.escape(menu_entity)}\""
            + rf"\s+detail_components={components_before}",
            60,
            "the component count came back",
            begin,
        )
        print("  [OK] component detach and attach round-trip")

        # One recorded create, then the selected entity's recorded delete
        # (the destroy and the selection clear as one step), then its undo:
        # the entity comes back under a fresh handle and the selection
        # follows the remap.
        count_before = latest_entity_count()
        assert count_before >= 1, "the rows heartbeat reported an empty world"
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.entity_create")
        session.wait(re.escape(NEW_ENTITY_TOKEN), 30, "the new entity queued", begin)
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="new entity"',
            30,
            "the new entity recorded",
            begin,
        )
        wait_applied(applied_before, "new entity applied")
        wait_for_entity_count(count_before + 1, "the create added one entity")

        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.entity_delete")
        session.wait(re.escape(DELETE_ENTITY_TOKEN), 30, "the delete queued", begin)
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="delete entity"',
            30,
            "the delete recorded",
            begin,
        )
        wait_applied(applied_before, "delete applied")
        wait_for_entity_count(count_before, "the delete removed one entity")
        session.wait(
            re.escape(EGUI_ROWS_TOKEN) + r'[^\n]*detail="-"',
            60,
            "the selection cleared with the delete",
            begin,
        )

        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("command editor.undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="delete entity"',
            30,
            "the delete undid",
            begin,
        )
        wait_applied(applied_before, "delete undo applied")
        wait_for_entity_count(count_before + 1, "the undo restored the entity")
        session.wait(
            re.escape(EGUI_ROWS_TOKEN) + r'[^\n]*detail="(?!-)[^"]+"',
            60,
            "the selection came back with the restore",
            begin,
        )
        print("  [OK] entity create, delete and undo round-trip")

        # Step 4d: the Preferences tab (Stage 20) - the file's rows through
        # the scripted channel, the live halves the reader applies (the
        # history limits and the snapping steps), and the per-project gizmo
        # entry the tool choices persist.
        preferences_path = session.sandbox / "RustHybridEcs" / "preferences.json"

        def wait_preference_value(key: str, expected: str, label: str) -> None:
            """Polls the disk until the preferences document's dotted `key` shows `expected`."""
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if preferences_path.is_file():
                    current = json.loads(preferences_path.read_text(encoding="utf-8"))
                    for segment in key.split("."):
                        current = current.get(segment) if isinstance(current, dict) else None
                        if current is None:
                            break
                    if current is not None and str(current) == expected:
                        print(f"  [OK] {label}")
                        return
                # Poll until the save lands on disk.
                time.sleep(0.2)
            raise TimeoutError(f"{label}: {key} is not {expected!r} in {preferences_path}")

        # The seed proves the startup read: the live reader applied its cap
        # and the tab's view carries its rows.
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN) + r"[^\n]*step_cap=400\s",
            30,
            "the seeded step cap applied",
        )
        begin = len(session.read_log())
        session.send("dump preferences")
        session.wait(re.escape(PREFERENCES_DUMP_TOKEN), 30, "preferences dumped", begin)
        dump = session.read_log()[begin:]
        assert re.search(r'snapping_translation="0\.25"', dump), dump[-600:]
        assert re.search(r'history_step_cap="400"', dump), dump[-600:]
        assert re.search(r'project="italian_brainrot"', dump), dump[-600:]
        print("  [OK] the seeded preferences reached the tab's view")

        # One row through the scripted save: the file, the tab's view and
        # the live reader all follow.
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        session.send("edit preference snapping_steps.rotation_degrees 7.5")
        session.wait(
            re.escape(PREFERENCE_EDIT_QUEUED_TOKEN), 30, "the preference edit queued", begin
        )
        session.wait(re.escape(PREFERENCES_SAVED_TOKEN), 30, "the preferences saved", begin)
        wait_applied(applied_before, "the preference row applied")
        wait_preference_value(
            "snapping_steps.rotation_degrees", "7.5", "the file carries the saved row"
        )
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN), 30, "the live reader reapplied", begin
        )
        begin = len(session.read_log())
        session.send("dump preferences")
        session.wait(re.escape(PREFERENCES_DUMP_TOKEN), 30, "preferences dumped again", begin)
        assert re.search(
            r'snapping_rotation_degrees="7\.5"', session.read_log()[begin:]
        ), session.read_log()[begin:][-600:]
        print("  [OK] a saved row reaches the file, the view and the reader")

        # The project's remembered entry persists with a tool change; the
        # saved translate step then steers the gizmo, so a snapped drag
        # moves by a multiple of it.
        begin = len(session.read_log())
        session.send("command editor.tool_translate")
        session.wait(
            re.escape(GIZMO_PREFERENCES_WRITTEN_TOKEN) + r'\s+project="italian_brainrot"',
            30,
            "the tool choice persisted",
            begin,
        )
        wait_preference_value(
            "gizmo.projects.italian_brainrot.mode",
            "translate",
            "the project entry carries the tool choice",
        )
        begin = len(session.read_log())
        session.send("edit preference snapping_steps.translation 0.3")
        session.wait(re.escape(PREFERENCES_SAVED_TOKEN), 30, "the translate step saved", begin)
        wait_preference_value(
            "snapping_steps.translation", "0.3", "the file carries the translate step"
        )
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN), 30, "the reader reapplied the step", begin
        )
        begin = len(session.read_log())
        session.send("command editor.tool_snap_toggle")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r"[^\n]*snapped=true",
            60,
            "snapping switched on",
            begin,
        )
        session.wait(
            re.escape(GIZMO_PREFERENCES_WRITTEN_TOKEN), 30, "the snap toggle persisted", begin
        )
        begin = len(session.read_log())
        session.send("gizmo drag x 30 0")
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN) + r'[^\n]*mode="translate"\s+axis="x"',
            60,
            "the snapped drag started",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'[^\n]*mode="translate"\s+axis="x"[^\n]*value=\[([-\d.eE+]+),',
            60,
            "the snapped drag ended",
            begin,
        )
        match = re.search(r"value=\[([-\d.eE+]+),", session.read_log()[begin:])
        snapped_delta = float(match.group(1))
        ratio = snapped_delta / 0.3
        assert abs(ratio - round(ratio)) < 0.01, (
            f"the snapped drag moved {snapped_delta}, not a multiple of 0.3"
        )
        print(f"  [OK] the snapped drag moved x by {snapped_delta} (a multiple of 0.3)")
        begin = len(session.read_log())
        session.send("command editor.tool_snap_toggle")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r"[^\n]*snapped=false",
            60,
            "snapping switched off",
            begin,
        )

        # The history limits are the file's other live half: a tiny cap
        # trims at once, and the restored cap stops the trimming - the
        # later steps' undo needs a working history.
        begin = len(session.read_log())
        session.send("edit preference history_step_cap 2")
        session.wait(re.escape(PREFERENCES_SAVED_TOKEN), 30, "the tiny cap saved", begin)
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN) + r"[^\n]*step_cap=2\s",
            30,
            "the tiny cap applied",
            begin,
        )
        session.wait(re.escape(HISTORY_TRIMMED_TOKEN), 30, "the history trimmed", begin)
        begin = len(session.read_log())
        session.send("edit preference history_step_cap 500")
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN) + r"[^\n]*step_cap=500\s",
            30,
            "the cap restored",
            begin,
        )
        print("  [OK] the history limits applied and trimmed live")

        # The project section: a tool choice persists under the project's
        # name, in the file and in the tab's view.
        begin = len(session.read_log())
        session.send("command editor.tool_rotate")
        session.wait(
            re.escape(GIZMO_PREFERENCES_WRITTEN_TOKEN) + r'\s+project="italian_brainrot"',
            30,
            "the second tool choice persisted",
            begin,
        )
        wait_preference_value(
            "gizmo.projects.italian_brainrot.mode", "rotate", "the project entry followed"
        )
        wait_preference_value("gizmo.mode", "rotate", "the last-used mode followed")
        begin = len(session.read_log())
        session.send("dump preferences")
        session.wait(re.escape(PREFERENCES_DUMP_TOKEN), 30, "preferences dumped once more", begin)
        assert re.search(
            r"remembered=true[^\n]*mode=Some\(Rotate\)", session.read_log()[begin:]
        ), session.read_log()[begin:][-600:]
        print("  [OK] the project's remembered tool round-trips")

        # Step 4d2: the editor's theme layers (W-0127 Stage 6). Three layers
        # sit over the code default - the user's file, named by the `theme`
        # preference and read from the state directory, and the project's file
        # under `res/editor/`, named by `editor_settings.yaml` - and the
        # project wins where both name the same item. Every step goes through
        # the files a person would edit; `dump theme` reports the merge the
        # shell installs.

        def write_theme_file(path: Path, guid: str, body) -> None:
            """Writes one standalone `.theme` document, the shape the loader reads."""
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(
                json.dumps(
                    {
                        "format_version": 1,
                        "asset_type": "pill_ui_theme::ThemeOverrides",
                        "guid": guid,
                        "asset": body,
                    },
                    indent=2,
                )
                + "\n",
                encoding="utf-8",
            )

        def dump_theme(label: str) -> str:
            """One `dump theme` line: the merge the shell installs."""
            text = session.request(
                "dump theme", re.escape(EDITOR_THEME_DUMP_TOKEN), 30, label
            )
            return [
                row for row in text.splitlines() if EDITOR_THEME_DUMP_TOKEN in row
            ][-1]

        def assert_theme(
            line: str, layers: str, accent: str, panel: str, label: str
        ) -> int:
            """Asserts one dump and returns its generation."""
            assert "layers={}".format(layers) in line, "{}: {}".format(label, line)
            assert "accent={}".format(accent) in line, "{}: {}".format(label, line)
            assert "panel={}".format(panel) in line, "{}: {}".format(label, line)
            print("  [OK] {}".format(label))
            return int(re.search(r"generation=(\d+)", line).group(1))

        # 1. The user's file: an accent and a panel fill over the code default.
        user_theme_path = session.sandbox / "RustHybridEcs" / "theme_user.json"
        user_theme_path.write_text(
            json.dumps({"palette": {"accent": "#ff488c", "panel": "#101820"}}),
            encoding="utf-8",
        )
        begin = len(session.read_log())
        session.send("edit preference theme theme_user.json")
        session.wait(re.escape(PREFERENCES_SAVED_TOKEN), 30, "the theme row saved", begin)
        session.wait(
            re.escape(EDITOR_THEME_RESOLVED_TOKEN) + r"\s+layers=code,user\s+generation=\d+",
            60,
            "the user layer merged",
            begin,
        )
        session.wait(
            re.escape(EDITOR_THEME_APPLIED_TOKEN)
            + r"\s+layers=code,user\s+user=\S*theme_user\.json",
            60,
            "the shell installed the user layer",
            begin,
        )
        user_generation = assert_theme(
            dump_theme("the user layer dumped"),
            "code,user",
            "#ff488c",
            "#101820",
            "the user file's accent and panel fill applied",
        )

        # 2. The project's own file names only the accent: it wins over the
        # user's accent and leaves the user's panel fill alone.
        write_theme_file(
            theme_root / "editor.theme",
            "000000000000000000000000000000b1",
            {"palette": {"accent": "#33d17a"}},
        )
        begin = len(session.read_log())
        session.wait(
            re.escape(EDITOR_THEME_RESOLVED_TOKEN)
            + r"\s+layers=code,user,project\s+generation=\d+",
            60,
            "the project layer merged",
            begin,
        )
        project_generation = assert_theme(
            dump_theme("the project layer dumped"),
            "code,user,project",
            "#33d17a",
            "#101820",
            "the project's accent won and the user's panel fill stayed",
        )
        assert project_generation > user_generation, (
            "the project layer did not raise the generation: {}".format(
                (user_generation, project_generation)
            )
        )

        # 3. Editing the project's file reaches the editor: the store
        # reimports it and the merge runs again.
        write_theme_file(
            theme_root / "editor.theme",
            "000000000000000000000000000000b1",
            {"palette": {"accent": "#0aa1ff"}},
        )
        begin = len(session.read_log())
        session.wait(
            re.escape(EDITOR_THEME_RESOLVED_TOKEN)
            + r"\s+layers=code,user,project\s+generation=\d+",
            60,
            "the edited project layer merged",
            begin,
        )
        edited_generation = assert_theme(
            dump_theme("the edited project layer dumped"),
            "code,user,project",
            "#0aa1ff",
            "#101820",
            "an edited project theme re-applied",
        )
        assert edited_generation > project_generation, (
            "the edited project file did not raise the generation: {}".format(
                (project_generation, edited_generation)
            )
        )

        # 4. `editor_settings.yaml` points the project layer at another file
        # without a restart.
        write_theme_file(
            theme_root / "alt.theme",
            "000000000000000000000000000000b2",
            {"palette": {"accent": "#d17a33"}},
        )
        editor_settings_path.write_text(
            'editor_theme: "editor/alt.theme"\n', encoding="utf-8"
        )
        begin = len(session.read_log())
        session.wait(
            re.escape(EDITOR_THEME_RESOLVED_TOKEN)
            + r"\s+layers=code,user,project\s+generation=\d+",
            60,
            "the named project layer merged",
            begin,
        )
        switched = dump_theme("the named project layer dumped")
        assert "project=editor/alt.theme" in switched, switched
        switched_generation = assert_theme(
            switched,
            "code,user,project",
            "#d17a33",
            "#101820",
            "the settings file switched the project layer live",
        )
        assert switched_generation > edited_generation, (
            "the switched layer did not raise the generation: {}".format(
                (edited_generation, switched_generation)
            )
        )

        # 5. A path outside `editor/` is refused with the rule, and the
        # remaining layers still apply.
        editor_settings_path.write_text('editor_theme: "ui/x.theme"\n', encoding="utf-8")
        begin = len(session.read_log())
        session.wait(
            re.escape(EDITOR_THEME_REFUSED_TOKEN)
            + r"project: [^\n]*must be under `editor/`",
            60,
            "the out-of-scope theme refused with the rule",
            begin,
        )
        session.wait(
            re.escape(EDITOR_THEME_RESOLVED_TOKEN) + r"\s+layers=code,user\s+generation=\d+",
            60,
            "the refused project layer skipped",
            begin,
        )
        refused = dump_theme("the refused project layer dumped")
        assert "project=none" in refused, refused
        assert_theme(
            refused,
            "code,user",
            "#ff488c",
            "#101820",
            "a refused project layer left the user's layer in force",
        )

        # 6. A name an earlier build understood names no file: it is reported
        # once, and the project layer still applies.
        editor_settings_path.unlink(missing_ok=True)
        begin = len(session.read_log())
        session.send("edit preference theme dark")
        session.wait(
            re.escape(EDITOR_THEME_LEGACY_TOKEN)
            + r" 'dark' is not a file; no user layer",
            60,
            "the legacy theme name reported",
            begin,
        )
        session.wait(
            re.escape(EDITOR_THEME_RESOLVED_TOKEN) + r"\s+layers=code,project\s+generation=\d+",
            60,
            "the project layer alone applied",
            begin,
        )
        assert_theme(
            dump_theme("the legacy name dumped"),
            "code,project",
            "#0aa1ff",
            "#1a1a1d",
            "a legacy theme name left the project layer alone",
        )

        # 7. A user file the merge refuses is named with its item, and the
        # project layer still applies: one broken file costs only itself.
        broken_path = session.sandbox / "RustHybridEcs" / "broken.theme"
        broken_path.write_text(
            json.dumps({"palette": {"accent": "not-a-colour"}}), encoding="utf-8"
        )
        begin = len(session.read_log())
        session.send("edit preference theme broken.theme")
        session.wait(
            re.escape(EDITOR_THEME_REFUSED_TOKEN)
            + r"user: [^\n]*palette\.accent",
            60,
            "the broken user layer refused with its item",
            begin,
        )
        assert_theme(
            dump_theme("the broken user layer dumped"),
            "code,project",
            "#0aa1ff",
            "#1a1a1d",
            "a refused user layer left the project's layer in force",
        )
        print("  [OK] the theme layers merge in order, live, and refuse individually")

        # 8. The project layer's font (W-0127 Stage 7). A `.ttf` under
        # `res/editor/` loads through the project's asset store and installs
        # ahead of the editor's own families; a path that ships nothing is
        # refused by name and leaves that family to the built-ins.
        font_directory = theme_root / "fonts"
        font_directory.mkdir(parents=True, exist_ok=True)
        (font_directory / "Silkscreen-Regular.ttf").write_bytes(
            (
                REPOSITORY_ROOT
                / "examples"
                / "circus_demo"
                / "res"
                / "fonts"
                / "Silkscreen-Regular.ttf"
            ).read_bytes()
        )
        write_theme_file(
            theme_root / "font.theme",
            "000000000000000000000000000000b3",
            {
                "palette": {"accent": "#33d17a"},
                "fonts": {"body": "editor/fonts/missing.ttf"},
            },
        )
        editor_settings_path.write_text(
            'editor_theme: "editor/font.theme"\n', encoding="utf-8"
        )
        begin = len(session.read_log())
        session.wait(
            re.escape(EDITOR_FONT_REFUSED_TOKEN) + r"[^\n]*missing\.ttf",
            60,
            "the missing font refused by name",
            begin,
        )
        # The font that is there installs, and the signature is not the one the
        # built-in families alone produce.
        installed_before = re.findall(
            r"signature=([0-9a-f]{16})", session.read_log()
        )[-1]
        write_theme_file(
            theme_root / "font.theme",
            "000000000000000000000000000000b3",
            {
                "palette": {"accent": "#33d17a"},
                "fonts": {"body": "editor/fonts/Silkscreen-Regular.ttf"},
            },
        )
        begin = len(session.read_log())
        session.wait(
            re.escape(EDITOR_FONTS_INSTALLED_TOKEN) + r"\s+signature=([0-9a-f]{16})",
            60,
            "the project's font installed",
            begin,
        )
        installed_after = re.findall(r"signature=([0-9a-f]{16})", session.read_log()[begin:])[-1]
        assert installed_after != installed_before, (
            "the installed font set did not change the signature: {}".format(
                (installed_before, installed_after)
            )
        )
        assert_theme(
            dump_theme("the project font dumped"),
            "code,project",
            "#33d17a",
            "#1a1a1d",
            "the font's layer kept its own accent and the editor's panel",
        )
        print("  [OK] a project font loads, installs and refuses with its name")
        font_generation = int(
            re.search(r"generation=(\d+)", dump_theme("the font step's last dump")).group(1)
        )

        # Step 4e: the Game tab's viewport (Stage 21) - the primary
        # surface's child window sits at the active tab's body, so the
        # project shows through it, and hides when another tab of the
        # group is active.
        def wait_game_rect(visible: bool, label: str) -> "re.Match":
            """Dumps the scene rects until the game line reports `visible`.

            The window follows the tab on the next layout pass, so a dump
            may reach the log before the placement did; each dump is its own
            anchored request, so no sleep stands in for the answer.
            """
            deadline = time.monotonic() + 30
            wanted = "true" if visible else "false"
            while time.monotonic() < deadline:
                scenes = session.request(
                    "dump scenes", SCENE_RECT_PATTERN, 30, "the scene dump"
                )
                for match in re.finditer(GAME_RECT_PATTERN, scenes):
                    if match.group(5) == wanted:
                        print(f"  [OK] {label}")
                        return match
            raise TimeoutError(f"{label}: the game view rect never reported visible={wanted}")

        begin = len(session.read_log())
        session.send("tab game")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="game"',
            30,
            "the game tab activated",
            begin,
        )
        game = wait_game_rect(True, "the game viewport placed and visible")
        assert int(game.group(3)) > 0 and int(game.group(4)) > 0, game.group(0)
        print(f"  [OK] the game viewport sits at {game.group(3)}x{game.group(4)}")
        begin = len(session.read_log())
        session.send("dump scenes")
        # Give the frame loop a moment to run the dump command.
        time.sleep(0.4)
        scene_lines = [
            match
            for match in re.finditer(SCENE_RECT_PATTERN, session.read_log()[begin:])
        ]
        assert scene_lines and all(match.group(6) == "false" for match in scene_lines), (
            "the scene viewports did not hide behind the game tab: {}".format(
                session.read_log()[begin:][-600:]
            )
        )
        print("  [OK] the scene viewports hide behind the game tab")
        begin = len(session.read_log())
        session.send("tab scene")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="scene"',
            30,
            "the scene tab back",
            begin,
        )
        wait_game_rect(False, "the game view hidden with its tab")

        # Step 4f: the floating toolbox (Stage 22) - the gizmo's tools, its
        # space and its snapping as a panel over the scene viewport. A row
        # click switches the tool without picking the scene, the panel is
        # draggable inside the viewport and keeps taking input where it
        # lands, and the second viewport carries its own.
        step_begin = len(session.read_log())
        session.wait(re.escape(TOOLBOX_RECT_TOKEN) + r"\s+view=0", 30, "the toolbox placed")
        session.wait(re.escape(TOOLBOX_ITEM_TOKEN), 30, "the toolbox rows reported")
        panel = toolbox_panel(session, 0)
        viewport = logged_scene_rect(session, 0)
        assert rect_contains(viewport, panel), (
            f"the toolbox {panel} is not inside the scene viewport {viewport[:4]}"
        )
        row = toolbox_row(session, 0, "translate")
        assert rect_contains(panel, row), f"the translate row {row} is outside the panel {panel}"
        print(f"  [OK] the toolbox floats at {panel} inside the viewport")

        # The rows act: each click queues the same `SetGizmoTool` edit the
        # viewport menu's items queue, and the anchor line carries the state
        # the edit left - the clicks read entry mode=rotate from step 4d, so
        # every switch below is a real change.
        begin = len(session.read_log())
        applied_before = occurrence_count(session, re.escape(ENGINE_COMMANDS_APPLIED_TOKEN))
        toolbox_click(session, 0, "translate", "the toolbox switched to translate")
        wait_applied(applied_before, "the toolbox tool applied")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r'[^\n]*mode="translate"',
            60,
            "the anchor carries the toolbox's tool",
            begin,
        )
        for item, expected, label in (
            ("snap", "snapped=true", "the toolbox turned snapping on"),
            ("snap", "snapped=false", "the toolbox turned snapping off"),
            ("space", 'space="local"', "the toolbox switched the space to local"),
            ("space", 'space="world"', "the toolbox switched the space back"),
        ):
            begin = len(session.read_log())
            toolbox_click(session, 0, item, label + " click")
            session.wait(re.escape(GIZMO_ANCHOR_TOKEN) + r"[^\n]*" + expected, 60, label, begin)
        print("  [OK] the toolbox rows switch the tool")

        # The handle drags the panel: the runner injects the press, the
        # move and the release one frame apart, and the shell logs where the
        # panel landed.
        handle = toolbox_row(session, 0, "handle")
        start_x, start_y = rect_center(handle)
        rect_before = toolbox_panel(session, 0)
        begin = len(session.read_log())
        session.send(f"drag {start_x:.0f} {start_y:.0f} {start_x + 120:.0f} {start_y + 80:.0f}")
        session.wait(re.escape(POINTER_DRAG_TOKEN), 30, "the drag completed", begin)
        session.wait(re.escape(TOOLBOX_MOVED_TOKEN), 30, "the toolbox moved", begin)
        panel_after = toolbox_panel(session, 0)
        moved_x = panel_after[0] - rect_before[0]
        moved_y = panel_after[1] - rect_before[1]
        assert abs(moved_x - 120) <= 2 and abs(moved_y - 80) <= 2, (
            f"the dragged toolbox moved by ({moved_x}, {moved_y}), not (120, 80)"
        )
        assert rect_contains(viewport, panel_after), (
            f"the dragged toolbox left the viewport: {panel_after}"
        )
        print(f"  [OK] the toolbox dragged to {panel_after}")

        # The moved panel still takes input where it landed.
        begin = len(session.read_log())
        toolbox_click(session, 0, "rotate", "the moved toolbox switched to rotate")
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN) + r'[^\n]*mode="rotate"',
            60,
            "the anchor followed the moved toolbox",
            begin,
        )

        # The second viewport carries its own toolbox, parked at its own
        # default corner (each scene view keeps its own position).
        begin = len(session.read_log())
        session.send("tab scene2")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="scene2"',
            30,
            "the second viewport's tab activated",
            begin,
        )
        session.wait(
            re.escape(TOOLBOX_RECT_TOKEN) + r"\s+view=1",
            30,
            "the second viewport's toolbox placed",
            begin,
        )
        session.send("dump scenes")
        # Give the frame loop a moment to run the dump command.
        time.sleep(0.4)
        second_view = logged_scene_rect(session, 1)
        assert second_view[4], "the second viewport did not become visible"
        second_panel = toolbox_panel(session, 1)
        assert rect_contains(second_view, second_panel), (
            f"the second toolbox {second_panel} is not inside its viewport {second_view[:4]}"
        )
        assert second_panel != panel_after, "the two viewports share one toolbox position"
        begin = len(session.read_log())
        session.send("tab scene")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="scene"',
            30,
            "the scene tab back",
            begin,
        )
        assert toolbox_panel(session, 0) == panel_after, (
            "the first toolbox did not keep the place the drag left it"
        )
        # No toolbox click picked the scene: every one landed in the panel's
        # own overlay region, whatever its position.
        picks = session.read_log()[step_begin:]
        assert "scene pick" not in picks, (
            "a toolbox click reached the scene: {}".format(picks[-500:])
        )
        print("  [OK] the second viewport's toolbox and the parked position")

        # The active viewport (Stage 28): a click inside a viewport makes it
        # the one camera navigation addresses, and navigation moves only
        # that view's camera.
        begin = len(session.read_log())
        session.send("tab scene2")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="scene2"',
            30,
            "the second viewport shown again",
            begin,
        )
        session.send("dump scenes")
        # Give the frame loop a moment to place the window and dump it.
        time.sleep(0.4)
        second_rect = logged_scene_rect(session, 1)
        corner_x = second_rect[0] + 6
        corner_y = second_rect[1] + 6
        begin = len(session.read_log())
        session.send(f"click {corner_x} {corner_y}")
        session.wait(
            re.escape(ACTIVE_VIEWPORT_TOKEN) + r"\s+view=1",
            30,
            "the second viewport activated",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump viewport")
        session.wait(
            re.escape(VIEWPORT_DUMP_TOKEN) + r'\s+active=1\s+interaction="idle"',
            30,
            "the second viewport is active",
            begin,
        )
        print("  [OK] a click activates the viewport it lands in")

        def viewport_cameras() -> dict:
            """The newest viewport dump's per-view camera rows."""
            begin = len(session.read_log())
            session.send("dump viewport")
            session.wait(re.escape(VIEWPORT_DUMP_TOKEN), 30, "the viewport dumped", begin)
            rows = re.findall(
                re.escape(VIEWPORT_CAMERA_TOKEN)
                + r'\s+view=(\d+)\s+eye="([-\d.,]+)"\s+pivot="([-\d.,]+)"'
                + r"\s+distance=([-\d.eE+]+)\s+yaw=([-\d.eE+]+)\s+pitch=([-\d.eE+]+)",
                session.read_log()[begin:],
            )
            return {int(row[0]): row[1:] for row in rows}

        before_navigation = viewport_cameras()
        begin = len(session.read_log())
        session.send("camera orbit 60 20")
        session.wait(
            re.escape(CAMERA_GESTURE_TOKEN) + r'\s+view=1\s+navigation="orbit"',
            30,
            "the orbit applied",
            begin,
        )
        session.send("camera pan 30 10")
        session.wait(
            re.escape(CAMERA_GESTURE_TOKEN) + r'\s+view=1\s+navigation="pan"',
            30,
            "the pan applied",
            begin,
        )
        session.send("camera dolly 25")
        session.wait(
            re.escape(CAMERA_GESTURE_TOKEN) + r'\s+view=1\s+navigation="dolly"',
            30,
            "the dolly applied",
            begin,
        )
        session.send("camera zoom 3")
        session.wait(
            re.escape(CAMERA_ZOOM_TOKEN) + r"\s+view=1",
            30,
            "the wheel zoom applied",
            begin,
        )
        after_navigation = viewport_cameras()
        assert after_navigation[0] == before_navigation[0], (
            "navigation moved the inactive viewport's camera: {} -> {}".format(
                before_navigation[0], after_navigation[0]
            )
        )
        assert after_navigation[1] != before_navigation[1], (
            "navigation left the active viewport's camera where it was"
        )
        print("  [OK] camera navigation moved the active viewport only")

        # Step 4e2: any number of scene views (W-0038 Stage 21). The Window
        # menu's "New Scene View" docks one more, and the frontend opens a
        # child window, a renderer surface and a camera for it - the same
        # three the two startup views get. The third view's camera then
        # moves on its own: the three cameras are independent.
        begin = len(session.read_log())
        session.send("command editor.scene_new_view")
        session.wait(
            re.escape(SCENE_VIEW_ADDED_TOKEN) + r'\s+tab="scene3"\s+view=2\b',
            30,
            "the third scene view",
            begin,
        )
        session.wait(
            re.escape(SCENE_VIEW_OPENED_TOKEN) + r"\s+view=2\s+surface=3",
            60,
            "the third view's child window",
            begin,
        )
        session.wait(
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+surface=3\b",
            60,
            "the third view's surface",
            begin,
        )
        session.send("dump scenes")
        session.wait(
            re.escape(SCENE_VIEW_RECT_TOKEN) + r"\s+view=2\b",
            30,
            "the third view's rectangle",
            begin,
        )
        session.wait(
            re.escape(TOOLBOX_RECT_TOKEN) + r"\s+view=2\b",
            30,
            "the third view's toolbox",
            begin,
        )
        third_rect = logged_scene_rect(session, 2)
        assert third_rect[4], "the third viewport did not become visible"
        print("  [OK] a third scene view opened its own window and toolbox")

        # The third viewport becomes the active one, so navigation may move
        # it; the other two keep their cameras.
        before_third = viewport_cameras()
        begin = len(session.read_log())
        session.send(f"click {third_rect[0] + 6} {third_rect[1] + 6}")
        session.wait(
            re.escape(ACTIVE_VIEWPORT_TOKEN) + r"\s+view=2",
            30,
            "the third viewport activated",
            begin,
        )
        session.send("camera orbit -40 15")
        session.wait(
            re.escape(CAMERA_GESTURE_TOKEN) + r'\s+view=2\s+navigation="orbit"',
            30,
            "the third viewport's orbit",
            begin,
        )
        after_third = viewport_cameras()
        assert len(after_third) >= 3, (
            "the viewport dump does not list three views: {}".format(after_third)
        )
        assert after_third[2] != before_third[2], (
            "the third viewport's camera did not move"
        )
        assert after_third[0] == before_third[0] and after_third[1] == before_third[1], (
            "the third viewport's navigation moved another view's camera"
        )
        print("  [OK] three scene views hold three independent cameras")

        # Back to the first viewport's tab: the steps below read the primary
        # scene's viewport and its dock.
        begin = len(session.read_log())
        session.send("tab scene")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="scene"',
            30,
            "the scene tab back again",
            begin,
        )

        # Step 4g: the infinite scene grid (Stage 23) - the view-bound scene
        # viewports draw it under the world; the toolbox's grid row flips the
        # renderer's one switch, and the resolution names only the scene
        # surfaces (the game viewport never draws it).
        step_begin = len(session.read_log())
        session.send("dump grid")
        session.wait(re.escape(SCENE_GRID_TOKEN), 30, "the grid dumped", step_begin)
        on_line = session.find(SCENE_GRID_PATTERN, step_begin)
        assert on_line, session.read_log()[begin:][-400:]
        assert on_line.group(1) == "true" and on_line.group(3) == "1,2,3", on_line.group(0)
        print(f"  [OK] the grid is on over the scene viewports ({on_line.group(3)})")
        begin = len(session.read_log())
        toolbox_click(session, 0, "grid", "the toolbox turned the grid off")
        session.wait(
            re.escape(SCENE_GRID_STATE_TOKEN) + r"\s+enabled=false",
            30,
            "the grid switched off",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump grid")
        session.wait(re.escape(SCENE_GRID_TOKEN), 30, "the grid dumped again", begin)
        off_line = session.find(SCENE_GRID_PATTERN, begin)
        assert off_line and off_line.group(1) == "false" and off_line.group(3) == "", (
            "the off grid still names surfaces: {}".format(
                off_line.group(0) if off_line else session.read_log()[begin:][-400:]
            )
        )
        print("  [OK] the grid switched off and stopped naming surfaces")
        begin = len(session.read_log())
        toolbox_click(session, 0, "grid", "the toolbox turned the grid back on")
        session.wait(
            re.escape(SCENE_GRID_STATE_TOKEN) + r"\s+enabled=true",
            30,
            "the grid switched back on",
            begin,
        )
        picks = session.read_log()[step_begin:]
        assert "scene pick" not in picks, (
            "a grid row click reached the scene: {}".format(picks[-500:])
        )
        print("  [OK] the grid toggles through the toolbox row")

        # Step 4h: the menu bar (W-0034) - the pill mark and the six titles
        # above the dock. Every item runs the action its panel runs, so the
        # assertions are the same tokens: a tab activated, a recorded create
        # and its undo, the snapping switch.
        bar = session.find(MENU_BAR_RECT_PATTERN)
        assert bar, "the menu bar never reported its rectangle"
        bar_rect = (float(bar.group(1)), float(bar.group(2)), float(bar.group(3)))
        bar_height = float(bar.group(4))
        assert bar_rect[1] < 10.0, f"the bar is not at the top of the window: {bar_rect}"
        assert bar_rect[2] > 200.0, f"the bar is too narrow to hold the menus: {bar_rect}"
        assert bar_height >= 20.0, f"the bar is too short to hold the mark: {bar_height}"
        titles = {name: menu_title_rect(session, name) for name in
                  ("file", "edit", "create", "view", "window", "help")}
        ordered = sorted(titles.items(), key=lambda item: item[1][0])
        assert [name for name, _ in ordered] == ["file", "edit", "create", "view", "window", "help"], (
            "the titles are out of Unity's order: {}".format(ordered)
        )
        first_title_left = titles["file"][0]
        # The mark sits at x=10 and is 18 wide, then the gap: a title inside
        # that room would be drawn over the logo.
        assert first_title_left >= 30.0, (
            "the mark's room is missing before the first title: {}".format(first_title_left)
        )
        print(f"  [OK] the bar holds the mark and six titles from x={first_title_left:.0f}")

        # The Window menu brings the Systems tab forward - the same
        # activation the scripted `tab` command takes.
        begin = len(session.read_log())
        menu_click(session, "window", "Systems", "Systems")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="systems"',
            30,
            "the Window menu activated the Systems tab",
            begin,
        )

        # Create -> New Entity records an entity; Edit -> Undo takes it back.
        count_before = latest_entity_count()
        begin = len(session.read_log())
        menu_click(session, "create", "New Entity", "New Entity")
        wait_for_entity_count(count_before + 1, "the Create menu's new entity")
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="new entity"',
            30,
            "the Create menu's step recorded",
            begin,
        )
        begin = len(session.read_log())
        menu_click(session, "edit", "Undo", "Undo")
        session.wait(
            re.escape(HISTORY_UNDO_TOKEN) + r'\s+label="new entity"',
            30,
            "the Edit menu undid it",
            begin,
        )
        wait_for_entity_count(count_before, "the undo removed the menu's entity")
        print("  [OK] the Create and Edit menus round-trip a recorded step")

        # View -> Snap flips the same switch the toolbox's row does, twice, so
        # the suite leaves it where it found it.
        begin = len(session.read_log())
        menu_click(session, "view", "Snap", "Snap")
        session.wait(
            re.escape(GIZMO_PREFERENCES_WRITTEN_TOKEN) + r"[^\n]*snap=true",
            30,
            "the View menu turned snapping on",
            begin,
        )
        begin = len(session.read_log())
        menu_click(session, "view", "Snap", "Snap")
        session.wait(
            re.escape(GIZMO_PREFERENCES_WRITTEN_TOKEN) + r"[^\n]*snap=false",
            30,
            "the View menu turned snapping back off",
            begin,
        )
        picks = session.read_log()[step_begin:]
        assert "scene pick" not in picks, (
            "a menu item click reached the scene: {}".format(picks[-500:])
        )
        print("  [OK] the View menu drives the gizmo's snapping, and its menus stay off the scene")

        # Step 4i: the Unity-style Inspector (W-0035) - one collapsible
        # section per component (a caret, a duotone icon, the label), typed
        # rows, and the Add Component button in the tab's top-right.
        begin = len(session.read_log())
        session.send("tab inspector")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="inspector"',
            30,
            "the Inspector tab shown",
            begin,
        )
        # A live handle: the rows heartbeat names the entity the Inspector
        # describes, and re-selecting it is the recorded path the entities
        # list takes.
        detail_entity = latest_detail_entity(session)
        begin = len(session.read_log())
        session.send(f"select entity {detail_entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(detail_entity)}",
            30,
            "an entity selected for the Inspector",
            begin,
        )
        section_begin = len(session.read_log())
        session.send("dump viewport")
        session.wait(re.escape(VIEWPORT_DUMP_TOKEN), 30, "a frame drawn", section_begin)
        # The rectangles log once per placement (and again whenever the
        # layout moves them), so a section drawn since the editor started is
        # read from the whole log, not from this window. The Inspector is a
        # narrow column that scrolls, so only its top is guaranteed to be
        # visible: the section whose logged header sits highest is the one a
        # scripted click can always reach.
        section_rects = [
            (
                match.group(1),
                float(match.group(2)),
                float(match.group(3)),
                float(match.group(4)),
                float(match.group(5)),
            )
            for match in re.finditer(INSPECTOR_SECTION_RECT_PATTERN, session.read_log())
        ]
        assert section_rects, "the Inspector never reported a section's rectangle"
        sections = [entry[0] for entry in section_rects]
        # Ties on the smallest y are broken by the latest log entry: the most
        # recent placement is the one the panel is showing.
        topmost = min(
            enumerate(section_rects), key=lambda entry: (entry[1][2], -entry[0])
        )[1]
        transform_section = topmost[0]
        print(
            f"  [OK] the Inspector draws {len(set(sections))} section(s), "
            f"toggling {transform_section}"
        )

        # A header click collapses its section; a second brings it back. The
        # click lands on the bar's upper half, which stays inside the panel's
        # visible viewport even when the header is partly scrolled out.
        def header_click_point() -> tuple:
            rect = inspector_section_rect(session, transform_section)
            return (rect[0] + 12.0, rect[1] + 6.0)

        center = header_click_point()
        begin = len(session.read_log())
        session.send(f"click {center[0]:.0f} {center[1]:.0f}")
        session.wait(
            re.escape(INSPECTOR_SECTION_TOKEN)
            + rf'\s+component="{re.escape(transform_section)}"\s+collapsed=true',
            30,
            "the section collapsed",
            begin,
        )
        # egui answers a click from the previous frame's widget rectangles, and
        # collapsing a section reshapes everything below it. A frame with no
        # click in flight lets the header settle as the pointer's target before
        # the second click arrives.
        settle = len(session.read_log())
        session.send("dump viewport")
        session.wait(re.escape(VIEWPORT_DUMP_TOKEN), 30, "a settling frame", settle)
        begin = len(session.read_log())
        center = header_click_point()
        session.send(f"click {center[0]:.0f} {center[1]:.0f}")
        session.wait(
            re.escape(INSPECTOR_SECTION_TOKEN)
            + rf'\s+component="{re.escape(transform_section)}"\s+collapsed=false',
            30,
            "the section expanded again",
            begin,
        )
        print("  [OK] a section collapses and expands from its header")

        # The Add Component button lists what the entity lacks; a pick adds
        # one recorded component, which the suite then removes again. The
        # click that opens the list can also land as a pick through the dock
        # and move the selection, so the count and the entity are read from a
        # heartbeat drawn for the opened list, not from before the click.
        add = rect_center(inspector_add_rect(session))
        begin = len(session.read_log())
        session.send(f"click {add[0]:.0f} {add[1]:.0f}")
        session.wait(re.escape(INSPECTOR_ADD_OPENED_TOKEN), 30, "the add list opened", begin)
        session.wait(
            INSPECTOR_ADD_ENTRY_PATTERN, 30, "the add list reported its entries", begin
        )
        entries = [
            match.group(1)
            for match in re.finditer(INSPECTOR_ADD_ENTRY_PATTERN, session.read_log()[begin:])
        ]
        assert entries, "the add list was empty"
        opened_at = session.read_log().rfind(INSPECTOR_ADD_OPENED_TOKEN)
        session.wait(
            re.escape(EGUI_ROWS_TOKEN),
            30,
            "a rows heartbeat after the add list opened",
            opened_at,
        )
        components_before = latest_detail_components(session)
        detail_entity = latest_detail_entity(session)
        chosen = entries[0]
        entry_rect_matches = [
            match
            for match in re.finditer(INSPECTOR_ADD_ENTRY_PATTERN, session.read_log()[begin:])
            if match.group(1) == chosen
        ]
        entry = entry_rect_matches[-1]
        entry_center = (
            float(entry.group(2)) + float(entry.group(4)) / 2.0,
            float(entry.group(3)) + float(entry.group(5)) / 2.0,
        )
        begin = len(session.read_log())
        session.send(f"click {entry_center[0]:.0f} {entry_center[1]:.0f}")
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="add ',
            30,
            "the add recorded a step",
            begin,
        )
        wait_for_detail_components(
            session,
            components_before + 1,
            f"the added component reached the section of {detail_entity}",
        )
        begin = len(session.read_log())
        session.send(f"edit remove {detail_entity} {chosen}")
        session.wait(
            re.escape(HISTORY_PUSH_TOKEN) + r'\s+label="remove ',
            30,
            "the suite removed the added component again",
            begin,
        )
        wait_for_detail_components(
            session, components_before, "the removal took it back out"
        )
        print(f"  [OK] the Add Component button added {chosen} and the suite removed it")

        # Step 4j: the field decorators (W-0036) - a component whose fields
        # declare [Header]/[Space]/[Tooltip]/[Range]/[Min]/[HideInInspector]
        # draws the rows the model chose: a separator line, a gap, a slider
        # carrying its tooltip, a clamped drag value, and no row at all for
        # the hidden field. `italian_brainrot`'s models carry the decorated
        # `ShowcaseComponent`, so the step picks one the way the scene
        # selection step does.
        begin = len(session.read_log())
        session.send("tab inspector")
        session.wait(
            re.escape(TAB_ACTIVATED_TOKEN) + r'\s+tab="inspector"',
            30,
            "the Inspector tab shown again",
            begin,
        )
        scenes = session.request(
            "dump scenes", SCENE_RECT_PATTERN, 30, "the scene rects dumped"
        )
        viewport = next(
            (
                tuple(int(value) for value in match.groups()[1:5])
                for match in re.finditer(SCENE_RECT_PATTERN, scenes)
                if match.group(1) == "0" and match.group(6) == "true"
            ),
            None,
        )
        assert viewport, "no visible scene view rect for the decorator step"
        showcase_rows = []
        for fraction_x, fraction_y in ((0.5, 0.5), (0.35, 0.4), (0.65, 0.4)):
            x = int(viewport[0] + viewport[2] * fraction_x)
            y = int(viewport[1] + viewport[3] * fraction_y)
            probe = len(session.read_log())
            session.send(f"click {x} {y}")
            session.send("dump viewport")
            session.wait(re.escape(VIEWPORT_DUMP_TOKEN), 30, "a frame drawn", probe)
            showcase_rows = inspector_rows(session, "ShowcaseComponent")
            if showcase_rows:
                break
        assert showcase_rows, (
            "no decorated component reached the Inspector; the scene probe never "
            "selected a model carrying ShowcaseComponent"
        )
        kinds = [kind for kind, _, _ in showcase_rows]
        assert "header" in kinds, f"no header row was drawn: {showcase_rows}"
        assert "space" in kinds, f"no gap row was drawn: {showcase_rows}"
        header = next(rest for kind, _, rest in showcase_rows if kind == "header")
        assert 'text="Frame"' in header, f"the header text did not reach the row: {header}"
        space = next(rest for kind, _, rest in showcase_rows if kind == "space")
        assert "pixels=6" in space, f"the gap height did not reach the row: {space}"
        slider = next(
            rest for kind, _, rest in showcase_rows
            if kind == "field" and 'field="lean"' in rest
        )
        assert 'widget="float-slider"' in slider, (
            f"the range decorator did not become a slider: {slider}"
        )
        assert 'tooltip="How far the shape leans towards the camera"' in slider, (
            f"the tooltip did not reach the row: {slider}"
        )
        clamped = next(
            rest for kind, _, rest in showcase_rows
            if kind == "field" and 'field="scale"' in rest
        )
        assert 'widget="float-drag"' in clamped, (
            f"the minimum decorator did not clamp the drag value: {clamped}"
        )
        assert not any(
            'field="internal_id"' in rest for _, _, rest in showcase_rows
        ), f"the hidden field drew a row: {showcase_rows}"
        print("  [OK] the Inspector drew the decorated rows and hid the hidden field")

        # A slider edit is one recorded step, like any other field write, and
        # the value it wrote answers the field dump in the *same* append: a
        # scripted line's batch is applied before the next line runs (W-0109).
        slider_entity = latest_detail_entity(session)
        begin = len(session.read_log())
        session.send_many(
            [
                f"edit set {slider_entity} ShowcaseComponent lean 0.25",
                f"dump field {slider_entity} ShowcaseComponent lean",
            ]
        )
        session.wait(re.escape(FIELD_TOKEN), 30, "the edited field", begin)
        dump = session.read_log()[begin:]
        assert re.search(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="set ShowcaseComponent\.lean"',
            dump,
        ), f"the slider edit recorded no step: {dump[-400:]}"
        assert "0.25" in dump, f"the edit did not reach the field: {dump[-400:]}"
        print("  [OK] a decorated field edit records and reads back in one append")

        # Step 5: the dock rearranges - an undocked tab becomes a second
        # dock surface - through the scripted channel.
        session.send("dump tabs")
        dump = wait_for_token(session, DOCK_DUMP_PATTERN, 30, "dock dump")
        assert re.search(r"surfaces=1\b", dump), f"the dock did not start with one surface: {dump}"
        session.send("undock entities")
        wait_for_token(session, re.escape(EGUI_TAB_UNDOCKED_TOKEN), 30, "undock")
        session.send("dump tabs")
        second = session.wait_occurrence(DOCK_DUMP_PATTERN, 2, 30, "dock dump after undock")
        assert re.search(r"surfaces=2\b", second), (
            "the undocked tab did not become a second dock surface: {}".format(second)
        )
        print("  [OK] dock rearranged, two surfaces")

        # Step 5b: a tab pops out into its own window (W-0038 Stage 23). The
        # window draws the tab in its own egui context, so the tab leaves the
        # dock's tree and keeps drawing; closing it or asking for a redock
        # brings it back. The window survives a shell reload, which Step 7
        # checks by reloading the shell while one is open.
        begin = len(session.read_log())
        session.send("popout inspector")
        session.wait(
            re.escape(POPOUT_MOVED_TOKEN) + r'\s+tab="inspector"\s+redock=false',
            30,
            "the Inspector popped out",
            begin,
        )
        session.wait(
            re.escape(POPOUT_OPENED_TOKEN) + r'\s+tab="inspector"',
            60,
            "the pop-out window opened",
            begin,
        )
        session.wait(
            re.escape(POPOUT_PRESENTED_TOKEN) + r'\s+tab="inspector"',
            60,
            "the pop-out window presented a frame",
            begin,
        )
        popout_dump = session.request(
            "dump tabs", DOCK_DUMP_PATTERN, 30, "dock dump with a pop-out"
        )
        assert re.search(
            re.escape(DOCK_POPOUT_TOKEN) + r'\s+tab="inspector"', popout_dump
        ), "the dock did not report the popped-out tab: {}".format(popout_dump[-600:])
        held = [
            tabs
            for tabs in re.findall(DOCK_LEAF_PATTERN, popout_dump)
            if "inspector" in tabs[3].split(",")
        ]
        assert not held, "the popped-out tab stayed in the dock's tree: {}".format(held)
        print("  [OK] the Inspector left the dock for its own window")

        # The way back: the same dock rule, driven from the scripted channel.
        begin = len(session.read_log())
        session.send("popout inspector redock")
        session.wait(
            re.escape(POPOUT_MOVED_TOKEN) + r'\s+tab="inspector"\s+redock=true',
            30,
            "the Inspector redocked",
            begin,
        )
        session.wait(
            re.escape(POPOUT_CLOSED_TOKEN) + r'\s+tab="inspector"',
            60,
            "the pop-out window closed",
            begin,
        )
        back = session.request("dump tabs", DOCK_DUMP_PATTERN, 30, "dock dump after redock")
        assert any(
            "inspector" in tabs[3].split(",")
            for tabs in re.findall(DOCK_LEAF_PATTERN, back)
        ), "the redocked tab did not return to the dock: {}".format(back[-600:])
        print("  [OK] the Inspector came back to the dock")

        # A scene view popped out needs a window of its own: a child window
        # can never leave the editor's monitor, so the frontend rebuilds the
        # view on a top-level window placed at the pop-out's own rectangle
        # (W-0038 Stage 23). Redocking gives it back its child window.
        begin = len(session.read_log())
        session.send("popout scene")
        session.wait(
            re.escape(SCENE_VIEW_REBOUND_TOKEN) + r"\s+surface=\d+\s+popped=true",
            60,
            "the popped-out scene view moved to a top-level window",
            begin,
        )
        session.wait(
            re.escape(POPOUT_PRESENTED_TOKEN) + r'\s+tab="scene"',
            60,
            "the scene pop-out presented a frame",
            begin,
        )
        session.send("popout scene redock")
        session.wait(
            re.escape(SCENE_VIEW_REBOUND_TOKEN) + r"\s+surface=\d+\s+popped=false",
            60,
            "the scene view went back to its child window",
            begin,
        )
        session.wait(
            re.escape(POPOUT_CLOSED_TOKEN) + r'\s+tab="scene"',
            60,
            "the scene pop-out closed",
            begin,
        )
        print("  [OK] the scene view followed its pop-out and came back")

        # One pop-out stays open across Step 7's shell reload: a generation
        # that does not know it must clear the window's egui data, or the new
        # shell reads a value the unloaded image created.
        begin = len(session.read_log())
        session.send("popout assets")
        session.wait(
            re.escape(POPOUT_MOVED_TOKEN) + r'\s+tab="assets"\s+redock=false',
            30,
            "the Assets tab popped out",
            begin,
        )
        session.wait(
            re.escape(POPOUT_PRESENTED_TOKEN) + r'\s+tab="assets"',
            60,
            "the Assets pop-out presented a frame",
            begin,
        )
        print("  [OK] a pop-out is open for the reload to keep")

        # Step 6: body edits patch live. A tab body calls its siblings by bare
        # name and `run_frame` reads a module constant and calls the menu bar,
        # so a patch crate only compiles because the crate root re-exports the
        # shell module: without it the host refuses the patch and reloads,
        # which is why this step demands LIVE where it used to accept either.
        highest = max(rows_frames(session))
        # The anchor carries the file's own line ending: the text is written
        # back byte for byte, and a translated anchor would either miss or
        # rewrite every line's ending.
        newline = "\r\n" if "\r\n" in original_tab_source else "\n"
        # Two lines inside the tab's row loop: the loop header and the first
        # statement of its body, so the probe lands where a statement is legal.
        tab_body = newline.join(
            (
                "        for (entity, label) in entity_rows(snapshot) {",
                "            let selected = selection.contains(entity);",
            )
        )
        assert tab_body in original_tab_source, "the Entities tab body moved; update the step"
        patched_tab = original_tab_source.replace(
            tab_body,
            newline.join(
                (
                    "        for (entity, label) in entity_rows(snapshot) {",
                    "            // A patch probe: one statement, inside the tab body only.",
                    "            let _hot_body_probe = 0u8;",
                    "            let selected = selection.contains(entity);",
                )
            ),
            1,
        )
        begin = len(session.read_log())
        session.edit_source(tab_entities_source, patched_tab)
        session.wait(
            HOT_PATCH_LIVE_PATTERN.format("entities_tab"),
            180,
            "the tab body patched live",
            begin,
        )
        highest = wait_for_frames_past(session, highest, 120, "frame counter continued")
        print(f"  [OK] tab body patched live, frame counter continued past {highest}")

        # The Systems tab's body names `icons` and `systems` by bare name, both
        # of which only `shell::prelude::*` publishes: a patch crate replaying
        # this file's opening compiles only because that glob is there. The
        # probe goes inside the header closure, where a statement is legal.
        systems_anchor = (
            "        let enabled = snapshot.systems.iter().filter(|row| row.enabled).count();"
        )
        assert (
            systems_anchor in original_systems_source
        ), "the Systems tab body moved; update the step"
        patched_systems = original_systems_source.replace(
            systems_anchor,
            newline.join(
                (
                    "        // A patch probe: one statement, inside the tab body only.",
                    "        let _hot_systems_probe = 0u8;",
                    systems_anchor,
                )
            ),
            1,
        )
        begin = len(session.read_log())
        session.edit_source(tab_systems_source, patched_systems)
        session.wait(
            HOT_PATCH_LIVE_PATTERN.format("systems_tab"),
            180,
            "the Systems tab body patched live",
            begin,
        )
        highest = wait_for_frames_past(session, highest, 120, "frame counter continued")
        print(
            "  [OK] Systems tab body patched live, frame counter continued past "
            f"{highest}"
        )

        # The same for a body in `run_frame`: the heartbeat cadence, a read of
        # `SLOW_UPDATE_MICROS` and a bare call to the menu bar all sit in it.
        cadence = re.search(r"is_multiple_of\((\d+)\)", original_source)
        assert cadence, "the shell's heartbeat cadence was not found"
        patched_frame = original_source.replace(
            cadence.group(0), "is_multiple_of({})".format(int(cadence.group(1)) + 1), 1
        )
        begin = len(session.read_log())
        session.edit_source(shell_source, patched_frame)
        session.wait(
            HOT_PATCH_LIVE_PATTERN.format("run_frame"),
            180,
            "the frame body patched live",
            begin,
        )
        print("  [OK] frame body patched live")

        # The tab dispatch itself: this body lived in `impl TabViewer for
        # EditorTabViewer<'_>` until it moved to the free `tab_body`, and the
        # scanner refuses an impl header carrying a lifetime, so a patch could
        # never address it there. The probe sits in the Scene arm, the first
        # one, where a statement is legal. This anchor is in the shell's own
        # file, so it carries that file's endings rather than the tab body's:
        # the two are written by different tools and need not agree.
        shell_newline = "\r\n" if "\r\n" in original_source else "\n"
        dispatch_anchor = shell_newline.join(
            (
                "        EditorTab::Scene(view) => {",
                "            let view = view as usize;",
            )
        )
        assert dispatch_anchor in patched_frame, "the tab dispatch moved; update the step"
        patched_dispatch = patched_frame.replace(
            dispatch_anchor,
            shell_newline.join(
                (
                    "        EditorTab::Scene(view) => {",
                    "            // A patch probe: one statement, inside the dispatch only.",
                    "            let _hot_dispatch_probe = 0u8;",
                    "            let view = view as usize;",
                )
            ),
            1,
        )
        begin = len(session.read_log())
        session.edit_source(shell_source, patched_dispatch)
        session.wait(
            HOT_PATCH_LIVE_PATTERN.format("tab_body"),
            180,
            "the tab dispatch patched live",
            begin,
        )
        highest = wait_for_frames_past(session, highest, 120, "frame counter continued")
        print(
            "  [OK] tab dispatch patched live, frame counter continued past "
            f"{highest}"
        )

        # Step 7: a structural edit of the shell reloads it; the rearranged
        # dock (a second surface) must survive the reload. An item added
        # outside every hot body cannot be patched at all, so the host reloads
        # and the waits below count occurrences rather than assuming how many
        # happened.
        structural = patched_dispatch + (
            "\n/// A marker the egui suite adds so the module grows an item and "
            "must reload, not patch.\npub fn egui_suite_marker() -> u32 {\n    1\n}\n"
        )
        # Every byte the reload writes lands after this offset, so the toast
        # waits below cannot be satisfied by an earlier patch's notice.
        toast_start = len(session.read_log())
        # Count both tokens BEFORE the edit: the reloaded module's own loaded
        # line is logged before the reload's processed line, so counting the
        # loaded token after waiting for the reload would already include it
        # and wait for one occurrence that never comes.
        reloads = occurrence_count(session, re.escape(EXTENSION_RELOAD_TOKEN))
        loads = occurrence_count(session, re.escape(EGUI_SHELL_LOADED_TOKEN))
        claims = occurrence_count(session, re.escape(SHELL_CONTEXT_CLAIMED_TOKEN))
        # The theme the executable resolved before the reload (W-0127
        # Stage 6): a reloaded shell seeds its applied generation from the
        # record, so it installs nothing again.
        applies = occurrence_count(session, re.escape(EDITOR_THEME_APPLIED_TOKEN))
        session.edit_source(shell_source, structural)
        session.wait_occurrence(
            re.escape(EXTENSION_RELOAD_TOKEN), reloads + 1, 180, "shell reload"
        )
        session.wait_occurrence(
            re.escape(EGUI_SHELL_LOADED_TOKEN), loads + 1, 180, "shell reloaded"
        )
        # The reloaded generation claims the shared egui context before it
        # draws: whatever the previous generation left in egui's temporary
        # data would be a read through an unloaded image (W-0037 Stage 9).
        session.wait_occurrence(
            re.escape(SHELL_CONTEXT_CLAIMED_TOKEN), claims + 1, 60, "context claim"
        )
        # Stage 4: the reload's own notice reaches the toast stack, with the
        # level the host assigned and the timing its build reported.
        session.wait(
            TOAST_PREAMBLE.format("info") + r'[^"]*reloaded in \d+ ms',
            60,
            "reload toast",
            toast_start,
        )
        after = session.request(
            "dump tabs", DOCK_DUMP_PATTERN, 30, "dock dump after shell reload"
        )
        assert re.search(r"surfaces=2\b", after), (
            "the shell reload reset the dock: {}".format(after)
        )
        highest = wait_for_frames_past(
            session, highest, 120, "frame counter survived the reload"
        )
        print("  [OK] shell reloaded, dock and counter kept")
        # The theme was not installed again: the record already held it, and
        # the reloaded generation read its generation from there.
        assert occurrence_count(session, re.escape(EDITOR_THEME_APPLIED_TOKEN)) == applies, (
            "the shell reload installed the theme again"
        )
        themed = dump_theme("theme after the shell reload")
        assert f"generation={font_generation}" in themed, (
            "the reloaded shell lost the resolved theme: {}".format(themed)
        )
        print("  [OK] the theme survived the shell reload without a re-install")

        # The pop-out that was open across the reload kept its window: the
        # new generation cleared its context before drawing into it, and the
        # dock model still lists the tab as popped out. One claim line covers
        # every context the session owns, so the wait is for the line naming
        # the popped-out window, by occurrence rather than by offset: the
        # generation before the reload had already claimed and never logged a
        # window.
        session.wait_occurrence(
            re.escape(SHELL_CONTEXT_CLAIMED_TOKEN) + r"\s+entries=\d+\s+windows=1\b",
            1,
            60,
            "the reloaded shell claimed the pop-out's context",
        )
        survived = session.request(
            "dump tabs", DOCK_DUMP_PATTERN, 30, "dock dump after the reload"
        )
        assert re.search(
            re.escape(DOCK_POPOUT_TOKEN) + r'\s+tab="assets"', survived
        ), "the reload lost the popped-out tab: {}".format(survived[-600:])

        # A window the reloaded generation opens presents like any other: the
        # pop-out machinery is the new image's code, not the old one's.
        begin = len(session.read_log())
        session.send("popout systems")
        session.wait(
            re.escape(POPOUT_OPENED_TOKEN) + r'\s+tab="systems"',
            60,
            "the reloaded shell opened a pop-out window",
            begin,
        )
        session.wait(
            re.escape(POPOUT_PRESENTED_TOKEN) + r'\s+tab="systems"',
            60,
            "the reloaded shell's pop-out presented a frame",
            begin,
        )
        # Both pop-outs come home, so the steps after this one see the dock
        # the earlier steps built.
        session.send("popout systems redock")
        session.send("popout assets redock")
        session.wait(
            re.escape(POPOUT_CLOSED_TOKEN) + r'\s+tab="assets"',
            60,
            "the surviving pop-out redocked",
            begin,
        )
        print("  [OK] the pop-out survived the reload and redocked")

        # Step 8: a project reload keeps the editor UI and its dock.
        project_text = project_source.read_bytes().decode("utf-8")
        session.edit_source(
            project_source, project_text + "\n// egui suite: project reload marker\n"
        )
        reloads = occurrence_count(session, re.escape(PROJECT_RELOAD_TOKEN))
        session.wait_occurrence(
            re.escape(PROJECT_RELOAD_TOKEN), reloads + 1, 240, "project reload"
        )
        after = session.request(
            "dump tabs", DOCK_DUMP_PATTERN, 30, "dock dump after project reload"
        )
        assert re.search(r"surfaces=2\b", after), (
            "the project reload reset the dock: {}".format(after)
        )
        highest = wait_for_frames_past(
            session, highest, 120, "frame counter survived the project reload"
        )
        print("  [OK] project reloaded, editor UI kept")

        # Step 8b: a build failure keeps the running generation and says so -
        # an error toast now, and a Problems row with the compiler's own
        # `file:line` (W-0038 Stages 4 and 6). The configured code editor is
        # a stub that records the argument it was opened with, so the click's
        # effect is a file this suite can read.
        session.write_stub_editor()
        failure_start = len(session.read_log())
        session.edit_source(
            project_source,
            project_text
            + "\n// egui suite: project reload marker\n"
            + 'compile_error!("egui suite deliberate failure");\n',
        )
        session.wait(
            TOAST_PREAMBLE.format("error") + r'[^"]*reload failed',
            240,
            "build-failed toast",
            failure_start,
        )
        # The same failure is a Problems row with the file and line the
        # compiler printed (W-0038 Stage 6), and a click on that row runs the
        # configured code editor command - here a stub that records its
        # arguments - with `file:line` as the last argument.
        problems_begin = len(session.read_log())
        session.send("tab problems")
        session.send("dump problems")
        session.wait(
            re.escape(PROBLEMS_DUMP_TOKEN) + r"\s+count=([1-9][0-9]*)",
            60,
            "the Problems rows",
            problems_begin,
        )
        row = re.search(
            re.escape(PROBLEM_ROW_TOKEN)
            + r'\s+module="[^"]*"\s+severity="error"\s+file="([^"]+)"\s+line=([1-9][0-9]*)',
            session.read_log()[problems_begin:],
        )
        assert row, "the Problems panel showed no located error row"
        assert row.group(1).endswith(project_source.name), (
            "the Problems row pointed at {!r}, not the edited source".format(row.group(1))
        )
        open_begin = len(session.read_log())
        session.send("problem open 0")
        session.wait(
            re.escape(PROBLEM_OPENED_TOKEN), 60, "the row opened in the editor", open_begin
        )
        recorded = wait_for_stub_record(session)
        # The log escapes a Windows path's backslashes; the stub writes what
        # it was given, so the expectation is unescaped before the compare.
        expected = row.group(1).replace("\\\\", "\\") + ":" + row.group(2)
        assert expected in recorded, (
            "the stub editor was opened with {!r}, not {!r}".format(recorded, expected)
        )
        print("  [OK] the Problems panel located the error and opened it")
        # Restoring the source builds again; the project comes back and the
        # failure's rows clear with it.
        session.edit_source(
            project_source, project_text + "\n// egui suite: project reload marker\n"
        )
        reloads = occurrence_count(session, re.escape(PROJECT_RELOAD_TOKEN))
        session.wait_occurrence(
            re.escape(PROJECT_RELOAD_TOKEN), reloads + 1, 240, "recovery reload"
        )
        cleared_begin = len(session.read_log())
        session.send("dump problems")
        session.wait(
            re.escape(PROBLEMS_DUMP_TOKEN) + r"\s+count=0",
            60,
            "the Problems rows cleared",
            cleared_begin,
        )
        print("  [OK] build-failed toast, then the project recovered")


        # Step 9: the second viewport's tab floats into its own egui window;
        # its own scene surface (view=1) goes live at that window's body
        # rectangle. The dock then holds three surfaces: the main one, the
        # Entities window from step 2, and the viewport's. The resize count
        # comes before the undock, because the undock's own resize (logged
        # right after the token) is the one this step waits for.
        view_one = (
            re.escape(SCENE_SURFACE_RESIZED_TOKEN)
            + r"\s+width=(\d+)\s+height=(\d+)\s+view=1"
        )
        before = occurrence_count(session, view_one)
        session.send("undock scene2")
        wait_for_token(
            session,
            re.escape(EGUI_TAB_UNDOCKED_TOKEN) + r'\s+tab="scene2"',
            30,
            "second viewport undock",
        )
        third = session.request(
            "dump tabs",
            DOCK_DUMP_PATTERN,
            30,
            "dock dump after the second viewport's undock",
        )
        assert re.search(r"surfaces=3\b", third), (
            "the second viewport's tab did not become a third dock surface: {}".format(third)
        )
        session.wait_occurrence(view_one, before + 1, 30, "second viewport surface")
        print("  [OK] second viewport undocked, its surface is live")

        # Step 10: Help -> About opens the About window, and File -> Quit
        # Editor leaves through the shell's own request - the graceful quit
        # and its clean exit code.
        menu_click(session, "help", "About Pill Editor", "About Pill Editor")
        print("  [OK] the Help menu opened the About window")
        # The entity copy the restart session pastes is taken here, one step
        # before the quit. The clipboard is the OS's - any other program can
        # replace it - so the copy is refreshed right before the editor leaves
        # instead of two minutes earlier, and read back to prove it landed.
        # The copy follows the selection, and the asset steps above left an
        # asset selected: a copy of that writes an asset payload, not an
        # entity. The handle comes from the list the editor has now, because
        # the reloads above renumber the display handles.
        listed = session.request(
            "dump entities",
            re.escape(ENTITIES_DUMP_TOKEN),
            30,
            "the entity list for the copy",
        )
        handles = re.findall(r'display="([^"]+)"', listed)
        assert handles, "no entity to copy: {}".format(listed[-400:])
        begin = len(session.read_log())
        session.send("select entity {}".format(handles[0]))
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + r'\s+entity=[^ ]+\s+count=1\b',
            30,
            "the entity re-selected for the copy",
            begin,
        )
        win_clipboard.clear()
        begin = len(session.read_log())
        session.send("command editor.copy")
        session.wait(
            re.escape(CLIPBOARD_WRITTEN_TOKEN), 30, "the second clipboard write", begin
        )
        copied = json.loads(win_clipboard.read_payload())
        assert copied.get("kind") == "entity", (
            "the entity copy did not reach the clipboard: {}".format(copied)
        )
        print("  [OK] an entity copy sits on the OS clipboard for the next editor")
        begin = len(session.read_log())
        menu_click(session, "file", "Quit Editor", "Quit Editor")
        session.wait(re.escape(QUIT_REQUESTED_TOKEN), 30, "the quit reached the frontend", begin)
        # The quit is where the rearranged dock is written: the undocked tab
        # and the second viewport each float on a surface of their own
        # (Stage 4).
        session.wait(
            re.escape(LAYOUT_SAVED_TOKEN) + r"\s+surfaces=3\b",
            30,
            "the layout saved on quit",
            begin,
        )
        status = session.wait_exit(timeout=60)
        assert status == 0, f"editor exited {status} on the menu's quit"
        assert "panic" not in session.read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0, the rearranged layout saved")
    finally:
        session.close()
        # The step's files (created, renamed, moved, restored, and their
        # sidecars) come off the project's disk; the staging probe too.
        for pattern in (
            "s16_stage.png*",
            "textures/s16_probe*",
            "textures/s16_renamed*",
            "models/s16_moved*",
        ):
            for path in asset_root.glob(pattern):
                path.unlink(missing_ok=True)
        # The staged material and the folder it needed go together; the
        # example ships no materials folder, so an empty one is the step's.
        staged_materials = asset_root / "materials"
        for path in staged_materials.glob("s16_reference*"):
            path.unlink(missing_ok=True)
        if staged_materials.is_dir() and not any(staged_materials.iterdir()):
            staged_materials.rmdir()
        # The settings step's sidecars return to their recorded bytes: the
        # engine's normalization reorders their keys even when the values
        # come back through undo.
        for meta_path, original in original_metas.items():
            meta_path.write_bytes(original)
        # The theme layers' files, the font they name and the project's editor
        # settings go together: the example ships none of them.
        if theme_root.is_dir():
            for theme_file in sorted(theme_root.rglob("*"), reverse=True):
                if theme_file.is_file():
                    theme_file.unlink(missing_ok=True)
                elif theme_file.is_dir():
                    theme_file.rmdir()
            theme_root.rmdir()
        editor_settings_path.unlink(missing_ok=True)

    # Step 11: the layout survives a restart (W-0026 Stage 4). The session
    # above quit through File -> Quit, which wrote the rearranged dock - the
    # second viewport floating on its own surface, in front - into the
    # sandbox; a second session on the same sandbox must open with it.
    # The copy the restart pastes crosses a process boundary through the OS
    # clipboard, so it is checked here as well: a payload another program
    # replaced reports itself, instead of surfacing as a timeout a minute into
    # the restarted editor.
    leftover = win_clipboard.read_payload()
    assert leftover and json.loads(leftover).get("kind") == "entity", (
        "the entity copy did not survive the quit: payload={!r} text={!r}".format(
            leftover, win_clipboard.read_text()
        )
    )
    restart = EditorSession(
        "egui",
        "restart",
        project=arguments.project,
        test_seconds=240,
        binary=binary,
        sandbox=session.sandbox,
    )
    restart.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "1"})
    try:
        wait_for_token(
            restart, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "restart startup"
        )
        wait_for_token(
            restart, re.escape(EGUI_SHELL_LOADED_TOKEN), 120, "restart shell load"
        )
        restored = restart.request(
            "dump tabs", DOCK_DUMP_PATTERN, 60, "the restored dock"
        )
        assert re.search(r"surfaces=3\b", restored), (
            "the rearranged dock did not survive the restart: {}".format(restored)
        )
        leaves = restart.read_log()
        floating = [
            match.group(0)
            for match in re.finditer(DOCK_LEAF_PATTERN, leaves)
            if (match.group(1), match.group(3)) in (("1", "entities"), ("2", "scene2"))
        ]
        assert len(floating) == 2, (
            "the floating surfaces did not come back with their tabs: {}".format(
                leaves[-2000:]
            )
        )
        print("  [OK] the rearranged dock came back after a restart")

        # The third scene view came back too (W-0038 Stage 21): the layout
        # carried its tab, so this process opened a window and a surface for
        # it before it drew anything.
        begin = len(restart.read_log())
        restart.send("dump scenes")
        restart.wait(
            re.escape(SCENE_VIEW_RECT_TOKEN) + r"\s+view=2\b",
            60,
            "the third view after the restart",
            begin,
        )
        assert "scene3" in restart.read_log(), (
            "the third scene view's tab did not survive the restart"
        )
        print("  [OK] the third scene view survived the restart")

        # The clipboard crossed the process boundary (W-0038 Stage 17): the
        # session above copied an entity onto the OS clipboard, and this
        # editor - a different process - pastes it as one recorded step.
        entities_here = len(
            re.findall(
                re.escape(ENTITY_ROW_TOKEN),
                restart.request(
                    "dump entities",
                    re.escape(ENTITIES_DUMP_TOKEN),
                    60,
                    "the restarted editor's entity list",
                ),
            )
        )
        begin = len(restart.read_log())
        restart.send("command editor.paste")
        restart.wait(
            re.escape(CLIPBOARD_PASTED_TOKEN) + r'\s+kind="entity"',
            60,
            "the pasted entity",
            begin,
        )
        pasted = len(
            re.findall(
                re.escape(ENTITY_ROW_TOKEN),
                restart.request(
                    "dump entities",
                    re.escape(ENTITIES_DUMP_TOKEN),
                    60,
                    "the entity list after the paste",
                ),
            )
        )
        assert pasted == entities_here + 1, (
            "the paste did not add exactly one entity: {} vs {}".format(
                pasted, entities_here
            )
        )
        begin = len(restart.read_log())
        restart.send("command editor.undo")
        restart.wait(
            re.escape(HISTORY_UNDO_TOKEN), 30, "the pasted entity's undo", begin
        )
        restored = len(
            re.findall(
                re.escape(ENTITY_ROW_TOKEN),
                restart.request(
                    "dump entities",
                    re.escape(ENTITIES_DUMP_TOKEN),
                    60,
                    "the entity list after the undo",
                ),
            )
        )
        assert restored == entities_here, (
            "the undo did not remove the pasted entity: {} vs {}".format(
                restored, entities_here
            )
        )
        print("  [OK] an entity copied in one editor pasted into another")

        # Leave through the scripted channel's own quit: the first click of a
        # freshly started session goes to focusing its window, and the menu's
        # quit path is the main session's check, not this one.
        begin = len(restart.read_log())
        restart.send("quit")
        restart.wait(
            re.escape(QUIT_REQUESTED_TOKEN), 30, "the restart's quit", begin
        )
        restart.wait(
            re.escape(LAYOUT_SAVED_TOKEN), 30, "the restart's layout save", begin
        )
        assert restart.wait_exit(timeout=60) == 0, "the restarted editor did not exit 0"
        print("  [OK] the restarted editor saved its layout and exited 0")
        # A clean exit leaves nothing to recover (W-0038 Stage 20).
        recovery_file = restart.editor_state_directory / "editor_egui_recovery.json"
        assert not recovery_file.exists(), (
            "a clean exit left a recovery document behind: {}".format(recovery_file)
        )
        print("  [OK] a clean exit removed the recovery document")
    finally:
        restart.close()

    # Step 11b: recovery after an unclean exit (W-0038 Stage 20). A killed
    # editor leaves its dock and selection in the recovery document; the next
    # run applies them and says so, and its own clean exit removes the file.
    recovered = EditorSession(
        "egui",
        "recovery",
        project=arguments.project,
        test_seconds=180,
        binary=binary,
    )
    recovered.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "1"})
    try:
        wait_for_token(
            recovered, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "recovery startup"
        )
        wait_for_token(
            recovered, re.escape(EGUI_SHELL_LOADED_TOKEN), 120, "recovery shell load"
        )
        # The document exists from the first frames; a change the autosave
        # watches (here an undock) rewrites it, which is what the survivor must
        # see, so the step waits for that write rather than for the file.
        recovery_file = recovered.editor_state_directory / "editor_egui_recovery.json"
        deadline = time.time() + 30
        while time.time() < deadline and not recovery_file.exists():
            # The editor writes the document on its own cadence; poll the
            # file system until it appears or the deadline passes.
            time.sleep(0.25)
        assert recovery_file.exists(), (
            "the running editor never wrote its recovery document: {}".format(
                recovery_file
            )
        )
        written_before = recovery_file.stat().st_mtime_ns
        recovered.send("undock entities")
        wait_for_token(
            recovered, re.escape(EGUI_TAB_UNDOCKED_TOKEN), 60, "the recovery undock"
        )
        deadline = time.time() + 30
        while time.time() < deadline and recovery_file.stat().st_mtime_ns == written_before:
            # The autosave also fires on change; wait for a newer mtime so the
            # comparison below cannot read the previous write.
            time.sleep(0.25)
        assert recovery_file.stat().st_mtime_ns != written_before, (
            "the undock did not reach the recovery document"
        )
        print("  [OK] the autosave recorded the rearranged dock")

        # The unclean exit: the process is killed, so nothing removes the file.
        process = recovered.process
        assert process is not None, "the recovery session did not start"
        process.kill()
        process.wait(timeout=60)
        recovered.close()
        assert recovery_file.exists(), (
            "the killed editor's recovery document went away: {}".format(recovery_file)
        )

        survivor = EditorSession(
            "egui",
            "recovered",
            project=arguments.project,
            test_seconds=180,
            binary=binary,
            sandbox=recovered.sandbox,
        )
        survivor.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "1"})
        try:
            wait_for_token(
                survivor,
                re.escape(RECOVERED_STATE_TOKEN),
                120,
                "the recovered state",
            )
            restored = survivor.request(
                "dump tabs", DOCK_DUMP_PATTERN, 60, "the recovered dock"
            )
            assert re.search(r"surfaces=2\b", restored), (
                "the rearranged dock did not come back from the recovery document: {}".format(
                    restored
                )
            )
            begin = len(survivor.read_log())
            survivor.send("quit")
            survivor.wait(
                re.escape(QUIT_REQUESTED_TOKEN), 30, "the survivor's quit", begin
            )
            assert survivor.wait_exit(timeout=60) == 0, (
                "the recovering editor did not exit 0"
            )
        finally:
            survivor.close()
        assert not recovery_file.exists(), (
            "the recovering editor's clean exit left the document behind"
        )
        print("  [OK] a killed editor's dock came back, and its clean exit cleared it")
    finally:
        recovered.close()

    # Step 12: the redraw cadence (W-0038 Stage 9). Without the continuous
    # switch, a paused idle editor sleeps between frames: two dumps three
    # seconds apart must not advance by the display's frame rate. Every other
    # session runs with the switch, so the older steps keep their pace.
    cadence = EditorSession(
        "egui", "cadence", project=arguments.project, test_seconds=180, binary=binary
    )
    cadence.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "0"})
    try:
        wait_for_token(
            cadence, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "cadence startup"
        )
        assert REDRAW_ON_DEMAND_TOKEN in cadence.read_log(), (
            "the editor never reported its redraw posture"
        )
        begin = len(cadence.read_log())
        cadence.send("command editor.pause_toggle")
        cadence.wait(
            re.escape(PAUSE_TOGGLED_TOKEN) + r"\s+paused=true",
            60,
            "the project paused for the cadence step",
            begin,
        )
        # A settled editor says so once, with the poll it settles into
        # (W-0049 Stage 1): a running project draws at the display rate and a
        # paused one stops drawing until something asks for a frame.
        cadence.wait(
            re.escape(FRAME_IDLE_TOKEN) + r"\s+poll_millis=250\b",
            60,
            "the editor settled into its idle poll",
            begin,
        )
        cadence_frames = []
        for index in range(2):
            begin = len(cadence.read_log())
            cadence.send("dump cadence")
            cadence.wait(re.escape(CADENCE_DUMP_TOKEN), 60, "the cadence dump", begin)
            dump = re.search(
                re.escape(CADENCE_DUMP_TOKEN)
                + r"\s+continuous=(?:true|false)\s+frames=(\d+)",
                cadence.read_log()[begin:],
            )
            assert dump is not None, "the cadence dump carried no frame count"
            cadence_frames.append(int(dump.group(1)))
            if index == 0:
                # The second dump is the one an idle stretch separates from
                # the first; the suite's own sleep is that stretch.
                time.sleep(3.0)
        idle_frames = cadence_frames[1] - cadence_frames[0]
        # A continuous loop draws at the display's pace - well over a hundred
        # frames in three seconds - while the idle poll owes twelve plus
        # whatever the host's own log lines changed.
        assert idle_frames <= 45, (
            "the editor drew {} frames in three idle seconds; the loop is not "
            "waiting (frames={})".format(idle_frames, cadence_frames)
        )
        print(
            "  [OK] the paused editor waited ({} frames in three seconds)".format(
                idle_frames
            )
        )
    finally:
        cadence.close()

    # Step 13: the no-overlay posture (W-0038 Stage 10). `PILL_EDITOR_NO_OVERLAY`
    # puts the UI on the window's own surface instead of the composition
    # overlay; the editor must still start, dock and edit, and the warning must
    # be said once rather than every frame.
    plain = EditorSession(
        "egui", "no-overlay", project=arguments.project, test_seconds=180, binary=binary
    )
    plain.start(
        extra_env={
            "PILL_EDITOR_NO_OVERLAY": "1",
            "PILL_EDITOR_CONTINUOUS_REDRAW": "1",
        }
    )
    try:
        wait_for_token(
            plain, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "no-overlay startup"
        )
        wait_for_token(
            plain, re.escape(EGUI_SHELL_LOADED_TOKEN), 120, "the shell in that posture"
        )
        log = plain.read_log()
        assert NO_OVERLAY_TOKEN in log, (
            "the editor never reported the no-overlay posture: {}".format(log[-800:])
        )
        assert log.count(NO_OVERLAY_TOKEN) == 1, (
            "the no-overlay warning repeated {} times".format(
                log.count(NO_OVERLAY_TOKEN)
            )
        )
        assert re.search(re.escape(OVERLAY_POSTURE_TOKEN) + r"\s+window_surface=true", log), (
            "the posture line did not name the window surface: {}".format(log[-800:])
        )
        # Docks: a tab is activated and undocked, and the dock reports the
        # second surface it became.
        plain.send("tab statistics")
        plain.send("undock statistics")
        dock = plain.request(
            "dump tabs", DOCK_DUMP_PATTERN, 60, "the dock in that posture"
        )
        surfaces = re.search(DOCK_DUMP_PATTERN, dock)
        assert surfaces and int(surfaces.group(1)) >= 2, (
            "the undocked tab did not become a surface: {}".format(dock[-600:])
        )
        assert "statistics" in dock, "the dock never named the undocked tab"
        # Edits: a preference row goes through the same queued path a click
        # takes, and reads back through the dump.
        begin = len(plain.read_log())
        plain.send("edit preference snapping_steps.rotation_degrees 9.5")
        plain.wait(
            re.escape(PREFERENCES_SAVED_TOKEN), 60, "the edit saved", begin
        )
        begin = len(plain.read_log())
        plain.send("dump preferences")
        plain.wait(re.escape(PREFERENCES_DUMP_TOKEN), 60, "the edit read back", begin)
        assert re.search(
            r'snapping_rotation_degrees="9\.5"', plain.read_log()[begin:]
        ), plain.read_log()[begin:][-600:]
        print("  [OK] the no-overlay posture started, docked and edited")
    finally:
        plain.close()

    # Step 14: module panels (W-0038 Stage 11). The shell module registers an
    # editor panel of its own; the tab docks, the layout remembers it by panel
    # id, and a panel whose module unloaded reads "module not loaded" rather
    # than calling into an image that is gone. The deliberately slow body
    # proves the budget: a badge on the tab and a warning toast.
    panels = EditorSession(
        "egui", "panels", project=arguments.project, test_seconds=240, binary=binary
    )
    panels.start(
        extra_env={
            "PILL_EDITOR_CONTINUOUS_REDRAW": "1",
            "PILL_TEST_PANEL_SLOW": "1",
        }
    )
    try:
        wait_for_token(
            panels, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "panel startup"
        )
        wait_for_token(
            panels,
            re.escape(PANEL_REGISTERED_TOKEN) + r'\s+id="shell\.demo"',
            120,
            "the demo panel registration",
        )
        dump = panels.request(
            "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the panel dump"
        )
        row = re.search(
            re.escape(PANEL_ROW_TOKEN)
            + r'\s+slot=(\d+)\s+id="shell\.demo"\s+source="[^"]+"\s+alive=true',
            dump,
        )
        assert row is not None, "the demo panel is not registered: {}".format(
            dump[-800:]
        )
        slot = row.group(1)
        # Docking it draws it every frame, which is what the slow body needs to
        # cross the 120-frame budget.
        begin = len(panels.read_log())
        panels.send("tab module:{}".format(slot))
        panels.wait(
            re.escape(PANEL_OVER_BUDGET_TOKEN), 180, "the over-budget panel", begin
        )
        dump = panels.request(
            "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the armed panel dump"
        )
        assert re.search(
            re.escape(PANEL_ROW_TOKEN)
            + r"\s+slot={}\s+.*?slow=true".format(slot),
            dump,
        ), "the panel's budget badge never armed: {}".format(dump[-800:])
        assert re.search(
            re.escape(TOAST_TOKEN)
            + r'\s+level="warning"\s+seq=\d+\s+text="Panel .* over its frame budget',
            panels.read_log(),
        ), "the over-budget panel queued no toast"
        print("  [OK] a slow module panel armed its badge and queued a toast")
        # The module unloads: its marker is dropped, and its panel reads so.
        begin = len(panels.read_log())
        panels.send("panel unload pill_editor_shell_egui_editor")
        panels.wait(re.escape(PANEL_UNLOADED_TOKEN), 60, "the unload", begin)
        dump = panels.request(
            "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the unloaded panel dump"
        )
        assert re.search(
            re.escape(PANEL_ROW_TOKEN) + r"\s+slot={}\s+.*?alive=false".format(slot),
            dump,
        ), "the unloaded panel still reads alive: {}".format(dump[-800:])
        assert "module not loaded" in dump or "Module not loaded" in panels.read_log(), (
            "the tab did not say the module is gone: {}".format(dump[-800:])
        )
        begin = len(panels.read_log())
        panels.send("panel restore pill_editor_shell_egui_editor")
        panels.wait(
            re.escape(PANEL_RESTORED_TOKEN), 60, "the restore", begin
        )
        assert re.search(
            re.escape(PANEL_RESTORED_TOKEN) + r".*?restored=[1-9][0-9]*",
            panels.read_log()[begin:],
        ), "the restore registered nothing: {}".format(panels.read_log()[begin:][-400:])
        dump = panels.request(
            "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the restored panel dump"
        )
        assert re.search(
            re.escape(PANEL_ROW_TOKEN) + r"\s+slot={}\s+.*?alive=true".format(slot),
            dump,
        ), "the restored panel did not come back: {}".format(dump[-800:])
        print("  [OK] a panel outlived its module's unload and came back")

        # Stages 12 and 13: the shell module's other two panels - the Spline
        # Tool, and the windows and modals a panel asks the shell for. The
        # demo panel's confirmation is driven through `panel window`, which
        # reaches the same code its own button does, and answered with Enter.
        dump = panels.request(
            "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the spline panel"
        )
        spline = re.search(
            re.escape(PANEL_ROW_TOKEN)
            + r'\s+slot=(\d+)\s+id="shell\.spline_tool"',
            dump,
        )
        assert spline is not None, (
            "the Spline Tool did not register: {}".format(dump[-800:])
        )
        spline_slot = spline.group(1)
        panels.send("tab module:{}".format(spline_slot))
        # The tab has to be drawn before its panel has a timing: the dump that
        # reads it is retried for a couple of frames rather than racing the
        # activation.
        drawn = None
        for _attempt in range(6):
            dump = panels.request(
                "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the drawn spline panel"
            )
            drawn = re.search(
                re.escape(PANEL_ROW_TOKEN)
                + r"\s+slot={}\s+.*?passes=(\d+)".format(spline_slot),
                dump,
            )
            if drawn is not None and int(drawn.group(1)) > 0:
                break
            # The panel's draw line arrives on its own frame; poll the log
            # until it appears or the loop's deadline ends it.
            time.sleep(0.5)
        assert drawn is not None and int(drawn.group(1)) > 0, (
            "the Spline Tool never drew: {}".format(dump[-800:])
        )
        print("  [OK] the Spline Tool registered and drew")

        # Stage 12: a control point is a leaf of the spline's struct array, and
        # a panel row writes it through the panel contract - a queued
        # `SetField`, applied at the frame boundary as a recorded edit. The row
        # itself is a drag value the test channel cannot press, so the write is
        # queued where the row puts it (`panel field`); the row-to-command
        # mapping is the shell crate's `spline_tool` unit tests, and what this
        # proves is the rest of the path: the queue, the recorded step and the
        # undo, on a real entity of a real project.
        dump = panels.request(
            "dump entities", re.escape(ENTITIES_DUMP_TOKEN), 60, "the entities"
        )
        showcase_row = next(
            (
                match
                for match in re.finditer(
                    re.escape(ENTITY_ROW_TOKEN)
                    + r'\s+display="([^"]+)"\s+components="([^"]*)"',
                    dump,
                )
                if SHOWCASE_COMPONENT in match.group(2)
            ),
            None,
        )
        assert showcase_row is not None, (
            "no entity carries the showcase component: {}".format(dump[-800:])
        )
        showcase_entity = showcase_row.group(1)
        panels.send("select entity {}".format(showcase_entity))
        leaf = "points[1].y"
        before = panels.request(
            "dump field {} {} {}".format(showcase_entity, SHOWCASE_COMPONENT, leaf),
            re.escape(FIELD_TOKEN),
            60,
            "the panel element's value",
        )
        original = re.search(r"value=(?:F32\()?(-?[0-9.]+)", before)
        assert original is not None, (
            "the panel element did not read back: {}".format(before[-600:])
        )
        begin = len(panels.read_log())
        panels.send("panel field {} {} 4.5".format(SHOWCASE_COMPONENT, leaf))
        panels.wait(
            re.escape(PANEL_FIELD_QUEUED_TOKEN), 60, "the panel's queued field write", begin
        )
        queued = panels.read_log()[begin:]
        assert re.search(
            r'component="[^"]*{}"\s+field="{}"\s+value="4\.5"'.format(
                re.escape(SHOWCASE_COMPONENT), re.escape(leaf)
            ),
            queued,
        ), "the panel queued a different write: {}".format(queued[-600:])
        panels.wait(
            re.escape(HISTORY_PUSH_TOKEN)
            + r'\s+label="set {}[^"]*"'.format(re.escape(SHOWCASE_COMPONENT)),
            60,
            "the panel edit recorded a step",
            begin,
        )
        begin = len(panels.read_log())
        panels.send(
            "dump field {} {} {}".format(showcase_entity, SHOWCASE_COMPONENT, leaf)
        )
        panels.wait(re.escape(FIELD_TOKEN), 60, "the panel-edited leaf", begin)
        assert "4.5" in panels.read_log()[begin:], (
            "the panel's write did not apply: {}".format(
                panels.read_log()[begin:][-600:]
            )
        )
        begin = len(panels.read_log())
        panels.send("command editor.undo")
        panels.wait(re.escape(HISTORY_UNDO_TOKEN), 60, "the panel write's undo", begin)
        begin = len(panels.read_log())
        panels.send(
            "dump field {} {} {}".format(showcase_entity, SHOWCASE_COMPONENT, leaf)
        )
        panels.wait(re.escape(FIELD_TOKEN), 60, "the restored panel leaf", begin)
        restored = re.search(r"value=(?:F32\()?(-?[0-9.]+)", panels.read_log()[begin:])
        assert restored is not None and float(restored.group(1)) == float(
            original.group(1)
        ), "the undo did not restore the panel's element: {}".format(
            panels.read_log()[begin:][-600:]
        )
        print("  [OK] a panel row's control-point write applied and undid")
        # The confirmation lives on the demo panel's tab, so that tab comes to
        # the front first: `panel window` stands in for clicking the panel's
        # own button, and a button is only clickable while its tab is shown.
        panels.send("tab module:{}".format(slot))
        begin = len(panels.read_log())
        panels.send("panel window {} demo.confirm".format(slot))
        panels.wait(
            re.escape(PANEL_WINDOW_REQUESTED_TOKEN), 60, "the window request", begin
        )
        panels.wait(
            re.escape(PANEL_WINDOW_OPENED_TOKEN) + r'\s+slot=\d+\s+window="demo\.confirm"',
            60,
            "the panel window",
            begin,
        )
        dump = panels.request(
            "dump windows", re.escape(WINDOWS_DUMP_TOKEN), 60, "the panel windows"
        )
        assert re.search(r"count=1\b", dump), (
            "the panel window is not on screen: {}".format(dump[-600:])
        )
        assert re.search(
            re.escape(WINDOW_ROW_TOKEN) + r'\s+id="demo\.confirm".*?modal=true', dump
        ), "the window is not modal: {}".format(dump[-600:])
        begin = len(panels.read_log())
        panels.send("key Enter")
        panels.wait(
            re.escape(PANEL_MODAL_ANSWERED_TOKEN) + r'\s+window="[^"]+"\s+accepted=true',
            60,
            "the modal answer",
            begin,
        )
        panels.wait(
            re.escape(PANEL_LOG_TOKEN) + r'\s+text="demo confirmed"',
            60,
            "the panel's confirmation",
            begin,
        )
        dump = panels.request(
            "dump windows", re.escape(WINDOWS_DUMP_TOKEN), 60, "the closed window"
        )
        assert re.search(r"count=0\b", dump), (
            "the answered modal is still on screen: {}".format(dump[-600:])
        )
        print("  [OK] a modal window opened, took Enter and closed")
        # A window dies with its module: open it again, unload the source, and
        # the shell drops it rather than drawing through a dead image.
        begin = len(panels.read_log())
        panels.send("panel window {} demo.confirm".format(slot))
        panels.wait(
            re.escape(PANEL_WINDOW_OPENED_TOKEN) + r'\s+slot=\d+\s+window="demo\.confirm"',
            60,
            "the reopened window",
            begin,
        )
        begin = len(panels.read_log())
        panels.send("panel unload pill_editor_shell_egui_editor")
        panels.wait(re.escape(PANEL_UNLOADED_TOKEN), 60, "the second unload", begin)
        dump = panels.request(
            "dump windows", re.escape(WINDOWS_DUMP_TOKEN), 60, "the unloaded windows"
        )
        assert re.search(r"count=0\b", dump), (
            "an unloaded module's window is still on screen: {}".format(dump[-600:])
        )
        begin = len(panels.read_log())
        panels.send("panel restore pill_editor_shell_egui_editor")
        panels.wait(
            re.escape(PANEL_RESTORED_TOKEN),
            60,
            "the second restore",
            begin,
        )
        begin = len(panels.read_log())
        panels.send("panel window {} demo.confirm".format(slot))
        panels.wait(
            re.escape(PANEL_WINDOW_OPENED_TOKEN) + r'\s+slot=\d+\s+window="demo\.confirm"',
            60,
            "the re-registered window",
            begin,
        )
        print("  [OK] a module's window closes on its unload and reopens after")
        begin = len(panels.read_log())
        panels.send("key Escape")
        panels.wait(
            re.escape(PANEL_MODAL_ANSWERED_TOKEN) + r'\s+window="[^"]+"\s+accepted=false',
            60,
            "the dismiss",
            begin,
        )
        print("  [OK] Escape dismisses a modal")
        # Stage 9 (W-0048): a scene saves, empties and opens again. Save
        # writes the live world to a new `.scene` under the project's asset
        # directory, New replaces it with nothing, and Open brings the same
        # entities back. The window title names the file the whole time.
        scene_path = "scenes/egui_suite.scene"
        scene_file = (MODULES_ROOT / arguments.project / "res" / scene_path).resolve()
        scene_file.unlink(missing_ok=True)
        begin = len(panels.read_log())
        panels.send("scene save {}".format(scene_path))
        panels.wait(re.escape(SCENE_SAVED_TOKEN), 60, "the scene save", begin)
        saved = re.search(
            re.escape(SCENE_SAVED_TOKEN)
            + r'\s+path="scenes/egui_suite\.scene"\s+guid=\S+\s+entities=(\d+)',
            panels.read_log()[begin:],
        )
        assert saved is not None, "the save did not name the scene it wrote"
        saved_count = int(saved.group(1))
        assert saved_count > 0, "the save wrote no entities"
        assert scene_file.is_file(), "the scene file was not written: {}".format(
            scene_file
        )
        panels.wait(
            re.escape(SCENE_TITLE_TOKEN) + r'\s+title="egui_suite\.scene"',
            60,
            "the title naming the saved scene",
            begin,
        )
        begin = len(panels.read_log())
        panels.send("scene new")
        panels.wait(re.escape(SCENE_NEW_TOKEN), 60, "the new scene", begin)
        removed = re.search(
            re.escape(SCENE_NEW_TOKEN) + r"\s+entities=(\d+)",
            panels.read_log()[begin:],
        )
        assert removed is not None and int(removed.group(1)) == saved_count, (
            "the new scene did not empty the world: {}".format(
                (removed.group(0) if removed else "no scene new line")
            )
        )
        begin = len(panels.read_log())
        panels.send("scene open {}".format(scene_path))
        panels.wait(re.escape(SCENE_OPENED_TOKEN), 60, "the scene open", begin)
        reopened = re.search(
            re.escape(SCENE_OPENED_TOKEN)
            + r'\s+path="scenes/egui_suite\.scene"\s+guid=\S+\s+entities=(\d+)'
            + r"\s+components=(\d+)",
            panels.read_log()[begin:],
        )
        assert reopened is not None, "the open did not name the scene it read"
        assert int(reopened.group(1)) == saved_count, (
            "the scene opened with {} of {} entities".format(
                reopened.group(1), saved_count
            )
        )
        assert int(reopened.group(2)) > 0, "the scene opened with no components"
        panels.wait(
            re.escape(SCENE_TITLE_TOKEN) + r'\s+title="egui_suite\.scene"',
            60,
            "the title naming the opened scene",
            begin,
        )
        print("  [OK] a scene saved, emptied and opened with its entities")
        # The scene is a suite artifact: it must not outlive the run that made
        # it, or the next start's asset scan finds it.
        scene_file.unlink(missing_ok=True)
        # Float the tab and leave: the layout writes the panel id beside it.
        panels.send("undock module:{}".format(slot))
        dock = panels.request(
            "dump tabs", DOCK_DUMP_PATTERN, 60, "the panel dock"
        )
        surfaces = re.search(DOCK_DUMP_PATTERN, dock)
        assert surfaces and int(surfaces.group(1)) >= 2, dock[-600:]
        assert any(
            "module" in match.group(4)
            for match in re.finditer(DOCK_LEAF_PATTERN, dock)
        ), "the panel tab never reached the dock: {}".format(dock[-600:])
        begin = len(panels.read_log())
        panels.send("quit")
        panels.wait(re.escape(QUIT_REQUESTED_TOKEN), 30, "the panel quit", begin)
        panels.wait(re.escape(LAYOUT_SAVED_TOKEN), 30, "the panel layout", begin)
        assert panels.wait_exit(timeout=60) == 0, "the panel editor did not exit 0"
    finally:
        panels.close()

    # The layout remembered the panel by id, not by slot: a second session on
    # the same sandbox opens with the module tab still there.
    panels_restart = EditorSession(
        "egui",
        "panels-restart",
        project=arguments.project,
        test_seconds=180,
        binary=binary,
        sandbox=panels.sandbox,
    )
    panels_restart.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "1"})
    try:
        wait_for_token(
            panels_restart,
            re.escape(EGUI_SHELL_LOADED_TOKEN),
            120,
            "the panel restart",
        )
        dump = panels_restart.request(
            "dump tabs", DOCK_DUMP_PATTERN, 60, "the restored panel dock"
        )
        assert any(
            "module" in match.group(4)
            for match in re.finditer(DOCK_LEAF_PATTERN, dump)
        ), "the module tab did not survive the restart: {}".format(dump[-800:])
        restored = panels_restart.request(
            "dump panels", re.escape(PANELS_DUMP_TOKEN), 60, "the restart's panels"
        )
        assert re.search(
            re.escape(PANEL_ROW_TOKEN) + r'\s+slot=\d+\s+id="shell\.demo"\s+.*?alive=true',
            restored,
        ), "the restarted editor did not register the panel: {}".format(
            restored[-800:]
        )
        print("  [OK] the module tab and its panel survived a restart")
        begin = len(panels_restart.read_log())
        panels_restart.send("quit")
        panels_restart.wait(re.escape(QUIT_REQUESTED_TOKEN), 30, "the restart's quit", begin)
        assert panels_restart.wait_exit(timeout=60) == 0, "the restarted panel editor did not exit 0"
    finally:
        panels_restart.close()

    # Stage 8: safe mode. The editor starts against a shell whose source does
    # not compile: it shows the build errors with their `file:line` and a
    # Retry, and once the source is fixed Retry loads the shell - all without
    # restarting the process.
    shell_source = (
        REPOSITORY_ROOT
        / "modules"
        / "extensions"
        / "pill_editor_shell_egui"
        / "pill_editor_shell_egui_editor"
        / "src"
        / "shell.rs"
    )
    shell_text = shell_source.read_bytes().decode("utf-8")
    safe = EditorSession(
        "egui", "safe", project=arguments.project, test_seconds=300, binary=binary
    )
    session.edit_source(
        shell_source, shell_text + '\ncompile_error!("egui suite: broken shell");\n'
    )
    # A staged DLL from the good build would load over the broken source and
    # there would be no safe mode to test: remove it so the failed build is the
    # only generation the editor can find. The retry below rebuilds it.
    staged_shell = MODULES_ROOT / "target" / "hot" / "pill_editor_shell_egui_editor.dll"
    staged_shell.unlink(missing_ok=True)
    try:
        safe.start(extra_env={"PILL_EDITOR_CONTINUOUS_REDRAW": "1"})
        try:
            wait_for_token(
                safe, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "safe-mode startup"
            )
            # The panel is drawn once, and the host's build failing can make it
            # appear before this suite reads `window created` - so the search
            # starts at the log's beginning rather than at a later anchor.
            safe.wait(re.escape(SAFE_MODE_TOKEN), 240, "the safe-mode panel")
            rows_begin = len(safe.read_log())
            rows = safe.request(
                "dump problems",
                re.escape(PROBLEMS_DUMP_TOKEN),
                60,
                "the safe-mode problems",
            )
            assert re.search(r"count=[1-9][0-9]*", rows), (
                "safe mode reported no build error: {}".format(rows)
            )
            located = re.search(
                re.escape(PROBLEM_ROW_TOKEN)
                + r'\s+module="[^"]*"\s+severity="error"\s+file="([^"]+)"\s+line=([1-9][0-9]*)',
                safe.read_log()[rows_begin:],
            )
            assert located and located.group(1).endswith("shell.rs"), (
                "the safe-mode panel did not name the shell's source: {}".format(located)
            )
            print("  [OK] safe mode showed the shell's error with its location")
            # Fix the source, then press the panel's Retry through the same
            # path the button calls.
            session.edit_source(shell_source, shell_text)
            retry_begin = len(safe.read_log())
            safe.send("safe_mode retry")
            safe.wait(
                re.escape(SAFE_MODE_RETRY_TOKEN), 60, "the safe-mode retry", retry_begin
            )
            safe.wait(
                re.escape(EGUI_SHELL_LOADED_TOKEN),
                480,
                "the shell loaded after the retry",
                retry_begin,
            )
            print("  [OK] Retry loaded the fixed shell without a restart")
        finally:
            safe.close()
    finally:
        # A failed step must not leave a compile error behind for the next run.
        session.edit_source(shell_source, shell_text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

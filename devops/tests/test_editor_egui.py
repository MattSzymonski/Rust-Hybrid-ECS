"""End-to-end acceptance for the egui editor frontend.

PROTECTS: The egui editor: startup, shell module, dock rearrangement, patch and reload state survival.
TIER: editor
SETS: editor

Builds against an already-built `pill_editor_egui` binary, starts it on
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
persisting into the project's remembered entry), the Game tab's viewport
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
    EDITOR_WINDOW_CREATED_TOKEN,
    run_directory,
)
from core.paths import MODULES_ROOT, REPOSITORY_ROOT, executable_name  # noqa: E402

# The suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources. Patterns that
# embed runtime values (frame counts, dock surfaces) compose them with their
# regex suffix at the wait.
EGUI_EDITOR_BINARY = executable_name("pill_editor_egui")
EGUI_SHELL_LOADED_TOKEN = "[editor] pill_editor_shell_egui_editor loaded"
EGUI_ROWS_TOKEN = "[editor] egui shell rows"
EGUI_DOCK_DUMP_TOKEN = "[editor] egui dock dump"
EGUI_TAB_UNDOCKED_TOKEN = "[editor] tab undocked"
SCENE_SURFACE_TOKEN = "[editor] scene surface attached"
SCENE_SURFACE_RESIZED_TOKEN = "[editor] scene surface resized"
FRAME_HEARTBEAT_TOKEN = "[editor] frame heartbeat"
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
HISTORY_REDO_TOKEN = "[editor] history redo"
ASSETS_DUMP_TOKEN = "[test] assets"
ASSET_ROW_TOKEN = "[test] asset"
BROWSER_FOLDER_TOKEN = "[editor] browser folder"
BROWSER_DUMP_TOKEN = "[test] browser"
BROWSER_ITEM_TOKEN = "[test] browser item"
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
PREFERENCES_DUMP_TOKEN = "[test] preferences"
PREFERENCES_APPLIED_TOKEN = "[editor] preferences applied"
PREFERENCES_SAVED_TOKEN = "[editor] preferences saved"
PREFERENCE_EDIT_QUEUED_TOKEN = "[editor] preference edit queued"
GIZMO_PREFERENCES_WRITTEN_TOKEN = "[editor] gizmo preferences written"
HISTORY_TRIMMED_TOKEN = "[editor] history trimmed"
GAME_VIEW_RECT_TOKEN = "[editor] game view rect"
TAB_ACTIVATED_TOKEN = "[editor] tab activated"
TOOLBOX_RECT_TOKEN = "[editor] toolbox rect"
TOOLBOX_ITEM_TOKEN = "[editor] toolbox item"
TOOLBOX_CLICK_TOKEN = "[editor] toolbox click"
TOOLBOX_MOVED_TOKEN = "[editor] toolbox moved"
POINTER_DRAG_TOKEN = "[editor] pointer drag injected"
SCENE_GRID_TOKEN = "[test] scene grid"
SCENE_GRID_STATE_TOKEN = "[editor] scene grid"

# The transform the gizmo drags edit; the field dump resolves the short
# name, like the native suite's dumps do.
TRANSFORM_COMPONENT = "TransformComponent"

# The rows heartbeat's runtime values, in the field order the shell logs them.
ROWS_PATTERN = (
    re.escape(EGUI_ROWS_TOKEN) + r"\s+frames=(\d+)\s+entities=(\d+)\s+systems=(\d+)"
)
DOCK_DUMP_PATTERN = re.escape(EGUI_DOCK_DUMP_TOKEN) + r"\s+surfaces=(\d+)"
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
    """Clicks one toolbox row at its logged center and waits for its token."""
    x, y = rect_center(toolbox_row(session, view, item))
    begin = len(session.read_log())
    session.send(f"click {x:.0f} {y:.0f}")
    session.wait(
        re.escape(TOOLBOX_CLICK_TOKEN) + rf'\s+view={view}\s+item="{item}"',
        30,
        label,
        begin,
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
        help="the built pill_editor_egui binary (default: the debug build)",
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
    # A concurrent cargo build rewrites the shared dylibs under the modules
    # this suite loads, which turns a clean start into a loader failure.
    cargo = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq cargo.exe", "/NH"],
        capture_output=True,
        text=True,
    ).stdout
    if "cargo.exe" in cargo:
        raise SystemExit(
            "a cargo build (cargo.exe) is running; wait for it to finish "
            "before running the editor suite - it rewrites the shared dylibs "
            "the modules load against"
        )
    # The default must be the egui binary: EditorSession's own default is the
    # native editor, whose binary is a different build and crashes at startup
    # against the egui editor's dylib split.
    binary = arguments.binary.resolve() if arguments.binary else EGUI_EDITOR_BINARY
    if not binary.is_file():
        raise SystemExit(
            "editor binary not found at {}; build it with "
            "`cargo build -p pill_editor_egui` in modules/ first".format(EGUI_EDITOR_BINARY)
        )
    shell_source = (
        MODULES_ROOT
        / "extensions"
        / "pill_editor_shell_egui"
        / "pill_editor_shell_egui_editor"
        / "src"
        / "shell.rs"
    )
    original_source = shell_source.read_text(encoding="utf-8")
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
    session.start()
    try:
        # Step 1: startup, all the way to both tabs reporting rows.
        wait_for_token(session, re.escape(EDITOR_WINDOW_CREATED_TOKEN), 120, "startup")
        wait_for_token(
            session,
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+detached=false",
            120,
            "scene surface",
        )
        # Both scene viewports attach at startup: the Scene tab's surface
        # and the second viewport's.
        session.wait_occurrence(
            re.escape(SCENE_SURFACE_TOKEN) + r"\s+detached=false",
            2,
            30,
            "second scene surface",
        )
        wait_for_token(session, re.escape(EGUI_SHELL_LOADED_TOKEN), 180, "shell load")
        rows = wait_for_token(session, ROWS_PATTERN, 180, "rows heartbeat")
        match = re.search(r"entities=(\d+)\s+systems=(\d+)", rows)
        assert match and int(match.group(1)) > 0, f"no entities in the rows: {rows}"
        print(f"  [OK] shell loaded, {rows}")

        def field_values(entity: str, field: str) -> "list[float]":
            """One array field of `entity`, from a scripted `dump field`."""
            start = len(session.read_log())
            session.send(f"dump field {entity} {TRANSFORM_COMPONENT} {field}")
            session.wait(re.escape(FIELD_TOKEN), 30, f"the {field} dump", start)
            dump = session.read_log()[start:]
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
        session.send("dump scenes")
        begin = len(session.read_log())
        scene_rect = None
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            match = re.search(SCENE_RECT_PATTERN, session.read_log()[begin:])
            if match and match.group(1) == "0" and match.group(6) == "true":
                scene_rect = tuple(int(value) for value in match.groups()[1:5])
                break
            # Poll until the frontend answers the dump.
            time.sleep(0.5)
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
        after_x = field_values(gizmo_entity, "translation")[0]
        assert abs(after_x - before_x) > 0.5, (
            f"the translate drag barely moved the entity: {before_x} -> {after_x}"
        )
        print(f"  [OK] translate drag moved x {before_x:.3f} -> {after_x:.3f}")

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

        # Step 4e: the Game tab's viewport (Stage 21) - the primary
        # surface's child window sits at the active tab's body, so the
        # project shows through it, and hides when another tab of the
        # group is active.
        def wait_game_rect(visible: bool, label: str) -> "re.Match":
            """Dumps the scene rects until the game line reports `visible`.

            The window follows the tab on the next layout pass, so a dump
            may reach the log before the placement did.
            """
            deadline = time.monotonic() + 30
            wanted = "true" if visible else "false"
            while time.monotonic() < deadline:
                begin = len(session.read_log())
                session.send("dump scenes")
                # Give the frame loop a moment to run the dump command.
                time.sleep(0.4)
                for match in re.finditer(GAME_RECT_PATTERN, session.read_log()[begin:]):
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

        # Step 4g: the infinite scene grid (Stage 23) - the view-bound scene
        # viewports draw it under the world; the toolbox's grid row flips the
        # renderer's one switch, and the resolution names only the scene
        # surfaces (the game viewport never draws it).
        step_begin = len(session.read_log())
        session.send("dump grid")
        session.wait(re.escape(SCENE_GRID_TOKEN), 30, "the grid dumped", step_begin)
        on_line = session.find(SCENE_GRID_PATTERN, step_begin)
        assert on_line, session.read_log()[begin:][-400:]
        assert on_line.group(1) == "true" and on_line.group(3) == "1,2", on_line.group(0)
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

        # Step 6: a live patch of the shell keeps the frame counter running.
        highest = max(rows_frames(session))
        cadence = re.search(r"is_multiple_of\((\d+)\)", original_source)
        assert cadence, "the shell's heartbeat cadence was not found"
        patched = original_source.replace(
            cadence.group(0), "is_multiple_of({})".format(int(cadence.group(1)) + 1), 1
        )
        session.edit_source(shell_source, patched)
        session.wait_either(
            [r"LIVE \d+ ms", re.escape(EXTENSION_RELOAD_TOKEN)], 180
        )
        highest = wait_for_frames_past(session, highest, 120, "frame counter continued")
        print(f"  [OK] shell patched, frame counter continued past {highest}")

        # Step 7: a structural edit of the shell reloads it; the rearranged
        # dock (a second surface) must survive the reload. The patch step's
        # edit may itself have reloaded instead of patching, so both waits
        # count occurrences rather than assuming how many happened.
        structural = original_source + (
            "\n/// A marker the egui suite adds so the module grows an item and "
            "must reload, not patch.\npub fn egui_suite_marker() -> u32 {\n    1\n}\n"
        )
        # Count both tokens BEFORE the edit: the reloaded module's own loaded
        # line is logged before the reload's processed line, so counting the
        # loaded token after waiting for the reload would already include it
        # and wait for one occurrence that never comes.
        reloads = occurrence_count(session, re.escape(EXTENSION_RELOAD_TOKEN))
        loads = occurrence_count(session, re.escape(EGUI_SHELL_LOADED_TOKEN))
        session.edit_source(shell_source, structural)
        session.wait_occurrence(
            re.escape(EXTENSION_RELOAD_TOKEN), reloads + 1, 180, "shell reload"
        )
        session.wait_occurrence(
            re.escape(EGUI_SHELL_LOADED_TOKEN), loads + 1, 180, "shell reloaded"
        )
        session.send("dump tabs")
        after = session.wait_occurrence(
            DOCK_DUMP_PATTERN,
            occurrence_count(session, DOCK_DUMP_PATTERN) + 1,
            30,
            "dock dump after shell reload",
        )
        assert re.search(r"surfaces=2\b", after), (
            "the shell reload reset the dock: {}".format(after)
        )
        highest = wait_for_frames_past(
            session, highest, 120, "frame counter survived the reload"
        )
        print("  [OK] shell reloaded, dock and counter kept")

        # Step 8: a project reload keeps the editor UI and its dock.
        project_text = project_source.read_text(encoding="utf-8")
        session.edit_source(
            project_source, project_text + "\n// egui suite: project reload marker\n"
        )
        reloads = occurrence_count(session, re.escape(PROJECT_RELOAD_TOKEN))
        session.wait_occurrence(
            re.escape(PROJECT_RELOAD_TOKEN), reloads + 1, 240, "project reload"
        )
        session.send("dump tabs")
        after = session.wait_occurrence(
            DOCK_DUMP_PATTERN,
            occurrence_count(session, DOCK_DUMP_PATTERN) + 1,
            30,
            "dock dump after project reload",
        )
        assert re.search(r"surfaces=2\b", after), (
            "the project reload reset the dock: {}".format(after)
        )
        highest = wait_for_frames_past(
            session, highest, 120, "frame counter survived the project reload"
        )
        print("  [OK] project reloaded, editor UI kept")

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
        session.send("dump tabs")
        third = session.wait_occurrence(
            DOCK_DUMP_PATTERN,
            occurrence_count(session, DOCK_DUMP_PATTERN) + 1,
            30,
            "dock dump after the second viewport's undock",
        )
        assert re.search(r"surfaces=3\b", third), (
            "the second viewport's tab did not become a third dock surface: {}".format(third)
        )
        session.wait_occurrence(view_one, before + 1, 30, "second viewport surface")
        print("  [OK] second viewport undocked, its surface is live")

        # Step 10: a graceful quit, with the clean exit code.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in session.read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0")
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
        # The settings step's sidecars return to their recorded bytes: the
        # engine's normalization reorders their keys even when the values
        # come back through undo.
        for meta_path, original in original_metas.items():
            meta_path.write_bytes(original)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

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
outline, the inspector reporting the picked entity's detail, a corner click
that clears both, scripted gizmo drags that translate, rotate and scale the
selected entity (checked through `dump field`), a dock rearrangement (an undocked
tab) through the scripted channel, a live patch of the shell that keeps its
frame counter running, a structural edit that reloads the shell and keeps the
rearranged dock, a project reload that keeps the editor UI, the second
viewport's undock with its own scene surface going live at the window's
rectangle, and a clean exit.

Each run keeps its log, sandbox and command file under
`local/editor-validation/egui-<run id>/`; the sandbox is imported by the
egui editor's layout persistence (`RustHybridEcs/editor_egui_layout.json`) so
a second run of the suite starts from the first run's layout. The only
sources this suite edits are the shell module's (restored when the session
closes) and the example project's; everything else it starts is its own
process tree.
"""
import argparse
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
# The two answers a scripted scene click can produce, as full patterns.
PICK_ENTITY_PATTERN = re.escape(SCENE_PICK_ENTITY_TOKEN) + r"\s+entity=(\S+)"
PICK_NOTHING_PATTERN = re.escape(SCENE_PICK_NOTHING_TOKEN)


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
    # The phases wait for two row heartbeats, a scene-selection probe, a
    # gizmo drag phase and a patch compile; the executable's seconds
    # deadline is a hang guard, kept comfortably above the phases even on a
    # slow machine.
    session = EditorSession(
        "egui",
        "main",
        project=arguments.project,
        test_seconds=360,
        binary=binary,
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

        def field_values(field: str) -> "list[float]":
            """One array field of the entity, from a scripted `dump field`."""
            start = len(session.read_log())
            session.send(f"dump field {gizmo_entity} {TRANSFORM_COMPONENT} {field}")
            session.wait(re.escape(FIELD_TOKEN), 30, f"the {field} dump", start)
            dump = session.read_log()[start:]
            match = re.search(r"value=Array\(\[([^\]]*)\]\)", dump)
            assert match, "the {} dump carried no array: {}".format(field, dump[-500:])
            return [
                float(value) for value in re.findall(r"F32\(([-\d.eE+]+)\)", match.group(1))
            ]

        # Translate: drag the X arrow 120 px right; the entity's x moves.
        before_x = field_values("translation")[0]
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
        after_x = field_values("translation")[0]
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
        scale = field_values("scale")
        print(f"  [OK] scale drag wrote {factor:.3f} (scale now {scale})")

        # Step 4: the dock rearranges - an undocked tab becomes a second
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

        # Step 5: a live patch of the shell keeps the frame counter running.
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

        # Step 6: a structural edit of the shell reloads it; the rearranged
        # dock (a second surface) must survive the reload. The step-3 edit
        # may itself have reloaded instead of patching, so both waits count
        # occurrences rather than assuming how many happened.
        structural = original_source + (
            "\n/// A marker the egui suite adds so the module grows an item and "
            "must reload, not patch.\npub fn egui_suite_marker() -> u32 {\n    1\n}\n"
        )
        session.edit_source(shell_source, structural)
        reloads = occurrence_count(session, re.escape(EXTENSION_RELOAD_TOKEN))
        session.wait_occurrence(
            re.escape(EXTENSION_RELOAD_TOKEN), reloads + 1, 180, "shell reload"
        )
        loads = occurrence_count(session, re.escape(EGUI_SHELL_LOADED_TOKEN))
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

        # Step 7: a project reload keeps the editor UI and its dock.
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

        # Step 8: the second viewport's tab floats into its own egui window;
        # its own scene surface (view=1) goes live at that window's body
        # rectangle. The dock then holds three surfaces: the main one, the
        # Entities window from step 2, and the viewport's.
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
        view_one = (
            re.escape(SCENE_SURFACE_RESIZED_TOKEN)
            + r"\s+width=(\d+)\s+height=(\d+)\s+view=1"
        )
        before = occurrence_count(session, view_one)
        session.wait_occurrence(view_one, before + 1, 30, "second viewport surface")
        print("  [OK] second viewport undocked, its surface is live")

        # Step 9: a graceful quit, with the clean exit code.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in session.read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0")
    finally:
        session.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

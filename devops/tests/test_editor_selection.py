"""End-to-end acceptance for scene selection.

PROTECTS: Scene selection: clicks pick entities through the pick contract; gizmo drags are undoable.
TIER: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/italian_brainrot with its master-renderer scene

DESCRIPTION
  Starts the editor on the Italian Brainrot project and drives scene selection
  and the transform gizmos through the renderer's pick contract:

    1. the Scene tab's rectangle is read from the shell's placement log;
    2. scripted clicks are aimed at a 3x3 grid inside it: at least one of them
       must hit an entity (`scene pick entity`), move the shared selection
       (`selection changed`, the same token a Hierarchy click logs) and publish
       the outline (`outline set`);
    3. a click on a corner of the view hits nothing, which clears the selection
       (`scene pick nothing`, `outline cleared`);
    4. a scripted `gizmo drag` presses on the translate X handle's own pixel
       and moves: the pick must answer a handle id (`scene pick handle`), the
       drag must run through the recorded path (one `history push`) and the
       world must hold what the drag logged;
    5. one undo puts the translation back; with snapping on the same drag lands
       on the 0.5 step; the rotate and scale tools each drag through their own
       handles, one history step each;
    6. the editor exits cleanly.

  Everything is asserted on the editor's log tokens; the suite runs under its
  own LOCALAPPDATA sandbox.

USAGE
  python devops/tests/test_editor_selection.py

  The run's log and command file land in their own folder under
  `local/editor-validation/selection-<run id>/`.

EXAMPLE USAGE
  python devops/tests/test_editor_selection.py

--- SCRIPT ---
"""

import re
import sys
import time
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_HISTORY_PUSH_TOKEN,
    EDITOR_HISTORY_UNDO_TOKEN,
    EDITOR_PANEL_PLACEMENT_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
SCENE_PICK_ENTITY_TOKEN = "[editor] scene pick entity"
SCENE_PICK_NOTHING_TOKEN = "[editor] scene pick nothing"
SCENE_PICK_HANDLE_TOKEN = "[editor] scene pick handle"
SELECTION_CHANGED_TOKEN = "[editor] selection changed"
OUTLINE_SET_TOKEN = "[editor] outline set"
OUTLINE_CLEARED_TOKEN = "[editor] outline cleared"
GIZMO_ANCHOR_TOKEN = "[editor] gizmo anchor"
GIZMO_DRAG_REQUEST_TOKEN = "[editor] test channel: gizmo drag request"
GIZMO_DRAG_PRESS_TOKEN = "[editor] gizmo drag press"
GIZMO_DRAG_STARTED_TOKEN = "[editor] gizmo drag started"
GIZMO_DRAG_ENDED_TOKEN = "[editor] gizmo drag ended"
GIZMO_TOOL_TOKEN = "[editor] gizmo tool"
GIZMO_PREFERENCES_TOKEN = "[editor] gizmo preferences written"
FIELD_UNREADABLE_TOKEN = "[test] field unreadable"

# The two answers a scripted scene click can produce, as full patterns: the
# probe loop waits for either.
PICK_ENTITY_PATTERN = re.escape(SCENE_PICK_ENTITY_TOKEN) + r"\s+entity=(\S+)"
PICK_NOTHING_PATTERN = re.escape(SCENE_PICK_NOTHING_TOKEN)


def scene_rect(session: EditorSession) -> "tuple[float, float, float, float]":
    """The Scene tab's dock rectangle from the shell's placement log."""
    match = re.search(
        re.escape(EDITOR_PANEL_PLACEMENT_TOKEN)
        + r'\s+kind="Scene"\s+x=([\d.]+)\s+y=([\d.]+)\s+'
        r'width=([\d.]+)\s+height=([\d.]+)',
        session.read_log(),
    )
    if not match:
        raise RuntimeError("no Scene placement in the log:\n" + session.read_log()[-3000:])
    return tuple(float(value) for value in match.groups())


def dump_array(
    session: EditorSession, entity: str, component: str, field: str
) -> "list[float]":
    """The F32 elements one `dump field` reports for an array field, in order."""
    begin = len(session.read_log())
    session.send(f"dump field {entity} {component} {field}")
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        text = session.read_log()[begin:]
        match = re.search(r"value=Array\(\[([^\]]+)\]\)", text)
        if match:
            return [float(value) for value in re.findall(r"F32\(([-\d.eE+]+)\)", match.group(1))]
        if FIELD_UNREADABLE_TOKEN in text:
            raise RuntimeError(f"{entity} has no readable {component}.{field}\n" + text[-2000:])
        process = session.process
        if process is not None and process.poll() is not None:
            raise RuntimeError(
                f"the editor exited {process.returncode} during a dump\n" + text[-3000:]
            )
        time.sleep(0.4)
    raise RuntimeError("the field dump never answered\n" + session.read_log()[-3000:])


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession(
        "selection", project="../examples/italian_brainrot", test_seconds=600
    )
    session.start()
    try:
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait(
            re.escape(EDITOR_PANEL_PLACEMENT_TOKEN) + r'\s+kind="Scene"',
            120,
            "scene docked",
        )

        # Step 1: the scene view's rectangle, in logical client pixels - the
        # space the scripted clicks use.
        x, y, width, height = scene_rect(session)
        print(f"  [OK] scene rect x={x} y={y} w={width} h={height}")

        # Step 2: probe a 3x3 grid; at least one cell must hit an entity. The
        # project's scene frames its entities, so the middle usually answers
        # first.
        picked_entity = None
        missed = None
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
            click_x = x + width * fraction_x
            click_y = y + height * fraction_y
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
            missed = (click_x, click_y)
            print(f"  [OK] click at ({click_x:.0f}, {click_y:.0f}) hit nothing, next")
        assert picked_entity, "no grid point hit an entity in the scene view"

        # The pick moved the shared selection and published the outline: both
        # are what the Hierarchy and the renderer read.
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

        # Step 3: a corner of the view holds nothing, which clears both.
        begin = len(session.read_log())
        session.send(f"click {x + 6:.0f} {y + 6:.0f}")
        session.wait_token(SCENE_PICK_NOTHING_TOKEN, 120, "the corner hits nothing", begin)
        session.wait_token(OUTLINE_CLEARED_TOKEN, 60, "the outline cleared with it", begin)

        # Step 4: the gizmos. The corner click cleared the
        # selection, so the grid's entity is selected again and its gizmo
        # must appear where the handles are drawn.
        begin = len(session.read_log())
        session.send(f"select {picked_entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the picked entity is selected again",
            begin,
        )
        session.wait(
            re.escape(GIZMO_ANCHOR_TOKEN)
            + rf'\s+entity={re.escape(picked_entity)}\s+mode="translate"\s+space="world"',
            120,
            "the translate handles are drawn for it",
            begin,
        )
        before = dump_array(session, picked_entity, "TransformComponent", "translation")
        print(f"  [OK] the entity's translation starts at {before}")

        # Step 5: one translate drag - press on the X handle's own pixel,
        # move 120 px right, release. The pick must answer a handle id, the
        # drag must write through the recorded path, and the whole drag must
        # be one undo step.
        begin = len(session.read_log())
        session.send("gizmo drag 0 120 0")
        session.wait_token(GIZMO_DRAG_REQUEST_TOKEN, 30, "the scripted drag was read", begin)
        session.wait(
            re.escape(GIZMO_DRAG_PRESS_TOKEN) + r'\s+axis="x"',
            60,
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
            + rf'\s+mode="translate"\s+axis="x"\s+entity={re.escape(picked_entity)}',
            60,
            "the drag started on the translate X handle",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),[^\n]*snapped=false",
            60,
            "the drag ended, unsnapped, with a value",
            begin,
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="gizmo translate"',
            60,
            "the drag is one history step",
            begin,
        )
        match = re.search(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="translate"\s+axis="x"\s+entity=\S+\s+value=\[([-\d.eE+]+),',
            session.read_log()[begin:],
        )
        logged_x = float(match.group(1))
        after = dump_array(session, picked_entity, "TransformComponent", "translation")
        assert abs(after[0] - logged_x) < 1e-3, f"the world holds {after[0]}, the drag logged {logged_x}"
        assert abs(after[0] - before[0]) > 1e-4, "the drag must move the entity's X"
        print(f"  [OK] the drag moved X {before[0]:.3f} -> {after[0]:.3f} (logged)")

        # Step 6: one undo puts it back - the drag was a single step.
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="gizmo translate"',
            60,
            "the undo step is the drag",
            begin,
        )
        deadline = time.monotonic() + 30
        while abs(dump_array(session, picked_entity, "TransformComponent", "translation")[0] - before[0]) > 1e-4:
            if time.monotonic() > deadline:
                raise AssertionError("the undo never restored the translation")
        print(f"  [OK] one undo restored X to {before[0]:.3f}")

        # Step 7: snapping (the Tools command, persisted), then the same
        # handle dragged far enough that the 0.5 step is certain to bite.
        begin = len(session.read_log())
        session.send("command editor.tool_snap_toggle")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r"[^\n]*snap=Some\(true\)",
            60,
            "snapping is on",
            begin,
        )
        session.wait(
            re.escape(GIZMO_PREFERENCES_TOKEN) + r"[^\n]*snap=true",
            60,
            "the choice is persisted",
            begin,
        )
        begin = len(session.read_log())
        session.send("gizmo drag 0 200 0")
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),[^\n]*snapped=true",
            60,
            "the snapped drag ended",
            begin,
        )
        match = re.search(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="translate"\s+axis="x"\s+entity=\S+\s+value=\[([-\d.eE+]+),',
            session.read_log()[begin:],
        )
        snapped_x = float(match.group(1))
        moved = snapped_x - before[0]
        assert abs(moved) > 1e-4, "the snapped drag must still move"
        assert abs(moved / 0.5 - round(moved / 0.5)) < 1e-3, f"{moved} is not on the 0.5 step"
        after = dump_array(session, picked_entity, "TransformComponent", "translation")
        assert abs(after[0] - snapped_x) < 1e-3, f"the world holds {after[0]}, the drag logged {snapped_x}"
        print(f"  [OK] the snapped drag moved X to {snapped_x:.3f} (step {moved:.1f})")

        # Step 8: the rotate tool: the Y ring is grabbed on its own plane and
        # the drag writes the whole quaternion, still as one step.
        begin = len(session.read_log())
        session.send("command editor.tool_rotate")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r"[^\n]*mode=Some\(Rotate\)",
            60,
            "the rotate tool is active",
            begin,
        )
        begin = len(session.read_log())
        session.send("gizmo drag 1 100 0")
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN)
            + rf'\s+mode="rotate"\s+axis="y"\s+entity={re.escape(picked_entity)}',
            60,
            "the drag started on the Y ring",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="rotate"\s+axis="y"\s+entity=\S+'
            r"\s+value=\[[-\d.eE+, ]+\]",
            60,
            "the ring drag ended with the quaternion",
            begin,
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="gizmo rotate"',
            60,
            "the ring drag is one history step",
            begin,
        )
        print("  [OK] the Y ring drag wrote the quaternion as one step")

        # Step 9: the scale tool, through its X handle; the written value is
        # the first element of the logged array.
        begin = len(session.read_log())
        session.send("command editor.tool_scale")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r"[^\n]*mode=Some\(Scale\)",
            60,
            "the scale tool is active",
            begin,
        )
        begin = len(session.read_log())
        session.send("gizmo drag 0 120 0")
        session.wait(
            re.escape(GIZMO_DRAG_STARTED_TOKEN)
            + rf'\s+mode="scale"\s+axis="x"\s+entity={re.escape(picked_entity)}',
            60,
            "the drag started on the scale X handle",
            begin,
        )
        session.wait(
            re.escape(GIZMO_DRAG_ENDED_TOKEN)
            + r'\s+mode="scale"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),",
            60,
            "the scale drag ended with a value",
            begin,
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="gizmo scale"',
            60,
            "the scale drag is one history step",
            begin,
        )
        before_scale = dump_array(session, picked_entity, "TransformComponent", "scale")
        print(f"  [OK] the scale drag wrote it as one step (scale now {before_scale})")

        # Leave the tool as it was found, and exit cleanly.
        begin = len(session.read_log())
        session.send("command editor.tool_translate")
        session.wait(
            re.escape(GIZMO_TOOL_TOKEN) + r"[^\n]*mode=Some\(Translate\)",
            60,
            "the translate tool is back",
            begin,
        )

        # Step 10: a clean exit.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"the editor exited {status}"
        print("  [OK] the editor exited cleanly")
        return 0
    finally:
        session.close()


if __name__ == "__main__":
    sys.exit(main())

"""
Editor Viewports Integration Test for Rust-Hybrid-ECS.

PROTECTS: Panels pop out into their own windows and redock; a popped-out
panel's arrangement outlives an unclean exit; several scene views draw at
once, each through its own camera, and picking and gizmos work in a later
view.
TIER: editor
SETS: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs (window scenarios), examples/italian_brainrot with
    its master-renderer scene (the view scenarios)

DESCRIPTION
    Two scenarios, one suite:

    1. Pop-out and arrangement (project_rs): the Preferences tab pops out
       into a window of its own (a built-in panel drawn through the dock's
       own build), the window is moved and resized through the scripted
       channel, and the sandbox's recovery document converges to hold that
       panel, size and position. Redocking closes the window and empties
       the floating list. A second pop-out, moved elsewhere, survives a
       kill: the restarted editor announces the recovery and reopens the
       window at the remembered position (`window created ... position=...`).
       The clean exit then discards the recovery document.

    2. Several scene views (italian_brainrot): with a second Scene tab
       opened into another tabset, the editor attaches and places a second
       scene surface (`view surface attached`), each view keeps its own
       editor camera (a scripted `key w view:<node>` moves only that view's
       camera), a scripted click inside the second view's rectangle asks a
       pick against that view's surface, and a scripted gizmo drag runs off
       that view's ray (the drag starts, which only happens when the pick
       names a handle on the second surface).

  Everything is asserted on the editor's log tokens and on the JSON the
  editor writes; each scenario runs under its own LOCALAPPDATA sandbox.

USAGE
  python devops/tests/test_editor_viewports.py

  The runs' logs and command files land in their own folder under
  `local/editor-validation/viewports-<run id>/`.

EXAMPLE USAGE
  python devops/tests/test_editor_viewports.py

--- SCRIPT ---
"""

import json
import re
import sys
import time
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_HISTORY_PUSH_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_TAB_SELECTED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
RECOVERY_RESTORED_TOKEN = "[editor] recovered editor state after an unclean exit"
PANEL_POPPED_OUT_TOKEN = "[editor] panel popped out"
PANEL_REDOCKED_TOKEN = "[editor] panel redocked"
WINDOW_OPENED_TOKEN = "[editor] window opened"
WINDOW_CLOSED_TOKEN = "[editor] window closed"
WINDOW_BUILT_TOKEN = "[editor] window panel built"
SURFACE_VIEW_ATTACHED_TOKEN = "[editor] view surface attached"
SCENE_SURFACE_ATTACHED_TOKEN = "[editor] scene surface attached"
VIEW_SHOWN_TOKEN = "[editor] view shown"
SCENE_VIEW_OPENED_TOKEN = "[editor] scene view opened"
VIEW_CAMERA_MOVED_TOKEN = "[editor] view camera moved"
VIEW_CAMERA_PUBLISHED_TOKEN = "[editor] view camera published"
EDITOR_CAMERA_MOVED_TOKEN = "[editor] editor camera moved"
SCENE_VIEW_CLICK_TOKEN = "[editor] scene view click"
PICK_REQUESTED_TOKEN = "[editor] pick requested"
PICK_ENTITY_TOKEN = "[editor] scene pick entity"
PICK_HANDLE_TOKEN = "[editor] scene pick handle"
PICK_NOTHING_TOKEN = "[editor] scene pick nothing"
GIZMO_DRAG_REQUEST_TOKEN = "[editor] test channel: gizmo drag request"
GIZMO_DRAG_STARTED_TOKEN = "[editor] gizmo drag started"
TEST_WINDOW_PLACE_TOKEN = "[editor] test window place"
TEST_WINDOW_RESIZE_TOKEN = "[editor] test window resize"
SELECTION_APPLIED_TOKEN = "[editor] selection applied"
TEST_ENTITY_TOKEN = "[test] entity"

RECOVERY_FILE_NAME = "editor_recovery.json"

# Any answer a pick can produce, as one pattern.
PICK_ANSWER_PATTERN = (
    "("
    + re.escape(PICK_ENTITY_TOKEN)
    + "|"
    + re.escape(PICK_HANDLE_TOKEN)
    + "|"
    + re.escape(PICK_NOTHING_TOKEN)
    + ")"
)

# `dump entities` answers one `[test] entity entity=<NvG> components=...`
# line per live entity.
ENTITY_PATTERN = re.escape(TEST_ENTITY_TOKEN) + r"\s+entity=(\S+)\s+components="

# A `view shown` line for one named view, with its logical rectangle.
def view_shown_pattern(view: str) -> str:
    """The `view shown` line of one view, its rectangle in the groups."""
    return (
        re.escape(VIEW_SHOWN_TOKEN)
        + r'\s+view="?'
        + re.escape(view)
        + r'"?\s+x=([\d.]+)\s+y=([\d.]+)\s+width=([\d.]+)\s+height=([\d.]+)'
    )


SHELL_LOAD_TIMEOUT = 300
GENERAL_TIMEOUT = 90
DOCUMENT_TIMEOUT = 30
QUIT_TIMEOUT = 120


def appdata_file(session: "EditorSession", name: str) -> Path:
    """One of the editor's per-user state files inside the sandbox."""
    return session.sandbox / "RustHybridEcs" / name


def read_recovery(path: Path) -> dict:
    """The recovery document, or an empty one while it is mid-write."""
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def wait_for_floating(
    path: Path,
    node: int,
    expected: "tuple[float, float, float, float] | None",
    timeout: float,
) -> dict:
    """Waits until the recovery document's floating list holds `node`.

    `expected` is `(width, height, x, y)` for a settled window, or `None`
    for the empty list a redock leaves behind.
    """
    deadline = time.monotonic() + timeout
    document = {}
    while time.monotonic() < deadline:
        document = read_recovery(path)
        floating = document.get("layout", {}).get("floating", [])
        if expected is None:
            if not floating:
                print("  [OK] the floating list is empty again")
                return document
        else:
            for entry in floating:
                if entry.get("tab") != node:
                    continue
                width, height, x, y = expected
                position = entry.get("position")
                if (
                    abs(entry.get("width", 0.0) - width) <= 1.0
                    and abs(entry.get("height", 0.0) - height) <= 1.0
                    and position
                    and abs(position[0] - x) <= 1.0
                    and abs(position[1] - y) <= 1.0
                ):
                    print(
                        "  [OK] the recovery document holds the window at "
                        "({}, {}, {}, {})".format(x, y, width, height)
                    )
                    return document
        # The shell writes the document when the arrangement changes; the
        # report arrives one frame after the scripted move.
        time.sleep(0.25)
    raise TimeoutError(
        "the floating list never settled:\n" + json.dumps(document, indent=2)[:2000]
    )


def run_window_scenario() -> None:
    """Pop-out, arrangement and restart (scenario 1)."""
    first = EditorSession("viewports", project="../examples/project_rs", test_seconds=600)
    recovery_file = appdata_file(first, RECOVERY_FILE_NAME)
    first.start()
    try:
        first.wait_token(EDITOR_SHELL_LOADED_TOKEN, SHELL_LOAD_TIMEOUT, "shell loaded")
        first.wait_token(EDITOR_WINDOW_CREATED_TOKEN, SHELL_LOAD_TIMEOUT, "editor started")

        start = len(first.read_log())
        first.send("tab Preferences")
        first.wait_token(EDITOR_TAB_SELECTED_TOKEN, GENERAL_TIMEOUT, "Preferences tab", start)

        # Pop the tab out: the shell opens a window for it and draws the
        # dock's own Preferences build inside.
        start = len(first.read_log())
        first.send("command editor.popout_tab")
        first.wait_token(PANEL_POPPED_OUT_TOKEN, GENERAL_TIMEOUT, "panel popped out", start)
        window_line = first.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?(popout:\d+)"?',
            GENERAL_TIMEOUT,
            "popout window opened",
            start,
        )
        text = first.read_log()[start:window_line]
        match = re.search(re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?(popout:\d+)"?', text)
        assert match, "the popout window's id is in its opened line"
        popout_id = match.group(1)
        node = int(popout_id.split(":")[1])
        first.wait_token(WINDOW_BUILT_TOKEN, GENERAL_TIMEOUT, "built-in panel built")
        first.wait(
            re.escape(WINDOW_BUILT_TOKEN)
            + r'\s+id="?'
            + re.escape(popout_id)
            + r'"?\s+panel="dock:preferences"',
            GENERAL_TIMEOUT,
            "the dock's Preferences build drew in the window",
            start,
        )

        # Move and resize it; the recovery document follows.
        start = len(first.read_log())
        first.send("window-place {} 220 140".format(popout_id))
        first.wait_token(TEST_WINDOW_PLACE_TOKEN, GENERAL_TIMEOUT, "window placed", start)
        start = len(first.read_log())
        first.send("window-resize {} 540 420".format(popout_id))
        first.wait_token(TEST_WINDOW_RESIZE_TOKEN, GENERAL_TIMEOUT, "window resized", start)
        wait_for_floating(recovery_file, node, (540.0, 420.0, 220.0, 140.0), DOCUMENT_TIMEOUT)

        # Redock: the window closes and the floating list empties.
        start = len(first.read_log())
        first.send("command editor.redock_tab")
        first.wait_token(PANEL_REDOCKED_TOKEN, GENERAL_TIMEOUT, "panel redocked", start)
        first.wait(
            re.escape(WINDOW_CLOSED_TOKEN) + r'\s+id="?'
            + re.escape(popout_id)
            + r'"?\s+reason="?redocked"?',
            GENERAL_TIMEOUT,
            "the popout window closed on redock",
            start,
        )
        wait_for_floating(recovery_file, node, None, DOCUMENT_TIMEOUT)

        # Pop it out again, arrange it elsewhere, and leave the process
        # killed: the arrangement has to outlive the crash.
        start = len(first.read_log())
        first.send("command editor.popout_tab")
        first.wait_token(PANEL_POPPED_OUT_TOKEN, GENERAL_TIMEOUT, "panel popped out again", start)
        window_line = first.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?(popout:\d+)"?',
            GENERAL_TIMEOUT,
            "second popout window opened",
            start,
        )
        match = re.search(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?(popout:\d+)"?',
            first.read_log()[start:window_line],
        )
        assert match, "the second popout window's id is in its opened line"
        popout_id = match.group(1)
        node = int(popout_id.split(":")[1])
        first.send("window-place {} 260 180".format(popout_id))
        first.send("window-resize {} 500 380".format(popout_id))
        wait_for_floating(recovery_file, node, (500.0, 380.0, 260.0, 180.0), DOCUMENT_TIMEOUT)

        process = first.process
        assert process is not None, "the session has not started"
        process.kill()
        process.wait(timeout=30)
        assert recovery_file.is_file(), "an unclean exit keeps the recovery document"
        print("  [OK] the killed editor left the arranged window in its recovery")
    finally:
        first.close()

    second = EditorSession(
        "viewports",
        session="second",
        project="../examples/project_rs",
        test_seconds=600,
        sandbox=first.sandbox,
    )
    second.start()
    try:
        second.wait_token(RECOVERY_RESTORED_TOKEN, SHELL_LOAD_TIMEOUT, "recovery announced")
        start = len(second.read_log())
        second.wait_token(WINDOW_OPENED_TOKEN, GENERAL_TIMEOUT, "popout window reopened", start)
        second.wait(
            re.escape(EDITOR_WINDOW_CREATED_TOKEN)
            + r'\s+id="?'
            + re.escape(popout_id)
            + r'"?.*position=Some\(\(260\.0, 180\.0\)\)',
            GENERAL_TIMEOUT,
            "the window reopened at the remembered position",
            start,
        )
        exit_code = second.quit_and_wait(timeout=QUIT_TIMEOUT)
        assert exit_code == 0, "expected a clean exit, got {}".format(exit_code)
        deadline = time.monotonic() + DOCUMENT_TIMEOUT
        while time.monotonic() < deadline and recovery_file.exists():
            # The clean exit removes the document before the process ends.
            time.sleep(0.25)
        assert not recovery_file.exists(), "a clean exit discards the recovery document"
        print("  [OK] the arrangement survived the restart; the clean exit discarded it")
    finally:
        second.close()


def run_view_scenario() -> None:
    """Two scene views with their own cameras, picks and gizmos (scenario 2)."""
    session = EditorSession("viewports", session="views", project="../examples/italian_brainrot", test_seconds=600)
    session.start()
    try:
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, SHELL_LOAD_TIMEOUT, "shell loaded")
        session.wait_token(SCENE_SURFACE_ATTACHED_TOKEN, SHELL_LOAD_TIMEOUT, "scene surface")
        session.wait(view_shown_pattern("scene"), GENERAL_TIMEOUT, "the scene view is placed")

        # A second Scene tab in another tabset: two views visible at once.
        start = len(session.read_log())
        session.send("tab Hierarchy")
        session.wait_token(EDITOR_TAB_SELECTED_TOKEN, GENERAL_TIMEOUT, "Hierarchy tab", start)
        start = len(session.read_log())
        session.send("command editor.scene_view_new")
        session.wait_token(SCENE_VIEW_OPENED_TOKEN, GENERAL_TIMEOUT, "scene view opened", start)
        attached = session.wait(
            re.escape(SURFACE_VIEW_ATTACHED_TOKEN) + r'\s+id="?(view:\d+)"?',
            GENERAL_TIMEOUT,
            "the second view surface attached",
            start,
        )
        match = re.search(
            re.escape(SURFACE_VIEW_ATTACHED_TOKEN) + r'\s+id="?(view:\d+)"?',
            session.read_log()[start:attached],
        )
        assert match, "the second view's id is in its attach line"
        view_id = match.group(1)
        node = int(view_id.split(":")[1])
        session.wait(view_shown_pattern(view_id), GENERAL_TIMEOUT, "the second view is placed")
        shown = re.search(view_shown_pattern(view_id), session.read_log())
        assert shown, "the second view's rectangle is in the log"
        rect_x, rect_y, rect_w, rect_h = (float(value) for value in shown.groups())
        assert rect_w >= 50.0 and rect_h >= 50.0, "the second view has a usable rectangle"
        print("  [OK] two scene views are attached and placed")

        # Each view owns its camera: the primary flies forward while the
        # second view strafes, so the two pivots end up in different places.
        second_start = len(session.read_log())
        session.send("key a view:{}".format(node))
        session.wait_token(
            VIEW_CAMERA_MOVED_TOKEN, GENERAL_TIMEOUT, "second view camera moved", second_start
        )
        session.wait(
            re.escape(VIEW_CAMERA_MOVED_TOKEN) + r'\s+view="?' + re.escape(view_id),
            GENERAL_TIMEOUT,
            "the moved camera is the second view's",
            second_start,
        )
        second_move = re.search(
            re.escape(VIEW_CAMERA_MOVED_TOKEN)
            + r'\s+view="?'
            + re.escape(view_id)
            + r'"?\s+position=\[([^\]]+)\]',
            session.read_log()[second_start:],
        )
        assert second_move, "the second camera's position is logged"
        second_position = second_move.group(1)

        start = len(session.read_log())
        session.send("key w scene")
        session.wait_token(EDITOR_CAMERA_MOVED_TOKEN, GENERAL_TIMEOUT, "primary camera moved", start)
        primary_move = re.search(
            re.escape(EDITOR_CAMERA_MOVED_TOKEN) + r'\s+reason=\S+\s+.*?position=\[([^\]]+)\]',
            session.read_log()[start:],
        )
        assert primary_move, "the primary camera's position is logged"
        assert primary_move.group(1) != second_position, "the two views keep different cameras"
        session.wait(
            re.escape(VIEW_CAMERA_PUBLISHED_TOKEN) + r'\s+view="?' + re.escape(view_id),
            GENERAL_TIMEOUT,
            "the second view's camera binding is published",
            second_start,
        )
        print("  [OK] each view moves and publishes its own camera")

        # A click inside the second view's own rectangle picks against its
        # surface, and the answer arrives.
        click_x = rect_x + rect_w * 0.5
        click_y = rect_y + rect_h * 0.5
        start = len(session.read_log())
        session.send("click {:.1f} {:.1f}".format(click_x, click_y))
        session.wait(
            re.escape(SCENE_VIEW_CLICK_TOKEN) + r'\s+.*view="?' + re.escape(view_id),
            GENERAL_TIMEOUT,
            "the click landed in the second view",
            start,
        )
        session.wait(
            re.escape(PICK_REQUESTED_TOKEN) + r'\s+view="?' + re.escape(view_id),
            GENERAL_TIMEOUT,
            "the pick asked the second view's surface",
            start,
        )
        session.wait(PICK_ANSWER_PATTERN, GENERAL_TIMEOUT, "the pick answered", start)

        # A gizmo drag off the second view's ray: the drag only starts when
        # the press pick names a handle on that surface.
        start = len(session.read_log())
        session.send("dump entities")
        session.wait(ENTITY_PATTERN, GENERAL_TIMEOUT, "entity listing", start)
        entity = re.search(ENTITY_PATTERN, session.read_log()[start:]).group(1)
        start = len(session.read_log())
        session.send("select {}".format(entity))
        session.wait_token(SELECTION_APPLIED_TOKEN, GENERAL_TIMEOUT, "entity selected", start)
        start = len(session.read_log())
        session.send("gizmo drag 0 25 0 view:{}".format(node))
        session.wait_token(GIZMO_DRAG_REQUEST_TOKEN, GENERAL_TIMEOUT, "drag requested", start)
        session.wait_token(GIZMO_DRAG_STARTED_TOKEN, GENERAL_TIMEOUT, "drag started off view 2", start)
        session.wait_token(EDITOR_HISTORY_PUSH_TOKEN, GENERAL_TIMEOUT, "drag recorded", start)
        print("  [OK] picking and gizmos work in the second view")
    finally:
        session.close()


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    print("Editor viewports suite")
    try:
        run_window_scenario()
        run_view_scenario()
    except (AssertionError, RuntimeError, TimeoutError) as error:
        print("  [FAIL] {}".format(error))
        return 1
    print("  PASS: pop-outs redock, arrangement survives a crash, views multiply")
    return 0


if __name__ == "__main__":
    sys.exit(main())

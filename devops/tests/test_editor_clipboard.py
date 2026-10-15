"""End-to-end acceptance for the clipboard.

PROTECTS: Clipboard and drag-and-drop: field copy and paste, entity round-trips and file drops.
TIER: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs

DESCRIPTION
  Starts the editor on the spline project and drives the clipboard through
  the scripted channel:

    1. widget text: a field's selected text is copied (Ctrl+C through the
       focused field) and pasted (Ctrl+V) into another field, one undo step;
    2. entities: `editor.copy` offers the selection as a structured payload
       (written to the system clipboard under the editor format with its
       readable text) and `editor.paste` creates an entity - one undo step;
    3. components: the Inspector's Copy button offers one component, and
       pasting it onto another entity writes its values - one undo step;
    4. file drops: a file hovering over the Assets panel previews, dropping
       copies it into the project's `res/`, the asset pipeline imports it
       with metadata, and undo removes the copy; two files dropped together
       are one step, and an unsupported type copies without metadata;
    5. drag out: starting an OS drag of a file enters OLE's drag loop and
       ends on posted input (the actual copy into Explorer is covered by
       the D26 manual checklist);
    6. cross-instance paste: a second, fresh editor on the same project
       pastes the entity the first one copied, from the system clipboard,
       with identical component values and one undo step.

  Everything is asserted on the editor's log tokens; the engine is paused so
  the project's systems cannot rewrite the values mid-phase. The suite runs
  under its own LOCALAPPDATA sandbox, and each instance keeps its log, command
  file and sandbox in `local/editor-validation/clipboard-<run id>/`.

USAGE
  python devops/tests/test_editor_clipboard.py

--- SCRIPT ---
"""

import base64
import ctypes
import re
import sys
import time
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_COMMAND_EXECUTED_TOKEN,
    EDITOR_COMMAND_REGISTRY_TOKEN,
    EDITOR_DRAG_OUT_BEGAN_TOKEN,
    EDITOR_DRAG_OUT_ENDED_TOKEN,
    EDITOR_HISTORY_PUSH_TOKEN,
    EDITOR_HISTORY_UNDO_TOKEN,
    EDITOR_PANEL_REGISTRY_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_TAB_SELECTED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)
from core.paths import REPOSITORY_ROOT  # noqa: E402

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
ENTITY_LIST_TOKEN = "[test] entity"
ENTITIES_COUNT_TOKEN = "[test] entities"
SELECTION_CHANGED_TOKEN = "[editor] selection changed"
WIDGET_ACTIVATED_TOKEN = "[editor] widget activated"
CLIPBOARD_SET_TEXT_TOKEN = "[editor] clipboard set text"
CLIPBOARD_READ_TEXT_TOKEN = "[editor] clipboard read text"
CLIPBOARD_COPIED_TOKEN = "[editor] clipboard copied"
CLIPBOARD_WROTE_OBJECT_TOKEN = "[editor] clipboard wrote object"
CLIPBOARD_READ_REQUESTED_TOKEN = "[editor] clipboard read requested"
CLIPBOARD_PASTE_ENTITY_TOKEN = "[editor] clipboard paste entity"
CLIPBOARD_PASTE_COMPONENT_TOKEN = "[editor] clipboard paste component"
DROP_PREVIEW_TOKEN = "[editor] drop preview shown"
IMPORT_QUEUED_TOKEN = "[editor] file drop import queued"
IMPORTED_ASSET_TOKEN = "[assets] imported a new asset"
TEST_CLICK_TOKEN = "[editor] test click"

ENTITY_ROW_PATTERN = re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=(\S+)\s"

# The drop phase's fixtures: a source file outside the project and the
# project's asset directory the import copies into.
DROP_SOURCE = REPOSITORY_ROOT / "local" / "temp" / "s18_drop.png"
ASSET_ROOT = REPOSITORY_ROOT / "examples" / "project_rs" / "res"
DROP_COPY = ASSET_ROOT / "s18_drop.png"


def entity_count(session: EditorSession) -> int:
    """The live entity count the latest `dump entities` reported."""
    matches = re.findall(
        re.escape(ENTITIES_COUNT_TOKEN) + r"\s+count=(\d+)", session.read_log()
    )
    if not matches:
        raise RuntimeError("no entities dump in the log\n" + session.read_log()[-2000:])
    return int(matches[-1])


def entity_ids(session: EditorSession) -> "set[str]":
    """The live entity ids this instance's fresh `dump entities` reports."""
    begin = len(session.read_log())
    session.send("dump entities")
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        ids = set(re.findall(ENTITY_ROW_PATTERN, session.read_log()[begin:]))
        if ids:
            return ids
        process = session.process
        if process is not None and process.poll() is not None:
            raise RuntimeError(
                f"the editor exited {process.returncode} during a dump\n"
                + session.read_log()[-3000:]
            )
        # Poll until the entity dump request answers.
        time.sleep(0.35)
    raise RuntimeError("the entity dump never answered\n" + session.read_log()[-3000:])


def dump_field(session: EditorSession, entity: str, component: str, field: str) -> str:
    """The value token one `dump field` reports, for any instance."""
    begin = len(session.read_log())
    session.send(f"dump field {entity} {component} {field}")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        match = re.search(r"value=([A-Za-z0-9]+)\(([^)]+)\)", session.read_log()[begin:])
        if match:
            return f"{match.group(1)}({match.group(2)})"
        # Poll until the field dump answers with a value.
        time.sleep(0.35)
    raise RuntimeError("the field dump never answered\n" + session.read_log()[-3000:])


def blur_focused_field(session: EditorSession) -> None:
    """Press the Scene tab strip's empty part, outside any widget.

    Stage 25's `editor.copy` is focus aware: with an Inspector field focused
    it copies the field's value, not the selected entity. The tab strip
    carries no interactive widget (only labels), so a press there claims
    nothing and the frames clear the field's focus.
    """
    begin = len(session.read_log())
    session.send("click 600 40")
    session.wait(
        re.escape(TEST_CLICK_TOKEN) + r"\s+x=600\.0\s+y=40\.0",
        30,
        "strip press cleared the field focus",
        begin,
    )


def end_the_ole_drag(editor_pid: int) -> None:
    """Post a mouse message into the editor's own windows.

    OLE's drag loop only re-checks the gesture when a message reaches one
    of the process's windows, and a scripted run holds no real button, so
    nothing else ever arrives; the loop's dialog-class window takes the
    message and the gesture then reads as ended. (Injecting desktop input
    does not work: without a real button down nothing captures the mouse,
    so the injected events never reach this process.)
    """
    user32 = ctypes.windll.user32
    our_windows: list[int] = []

    @ctypes.WINFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
    def collect(hwnd, _param):
        owner = ctypes.c_ulong()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == editor_pid:
            our_windows.append(hwnd)
        return 1

    user32.EnumWindows(collect, 0)
    wm_mousemove = 0x0200
    for hwnd in our_windows:
        user32.PostMessageW(hwnd, wm_mousemove, 0, 0)


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession("clipboard", test_seconds=900)
    session.start()
    second = None
    try:
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait_token(EDITOR_COMMAND_REGISTRY_TOKEN, 120, "registry ready")
        session.wait(
            re.escape(EDITOR_PANEL_REGISTRY_TOKEN) + r"\s+panels=\d+",
            120,
            "panels ready",
        )

        # The project rewrites component fields every tick; pause first.
        begin = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            begin,
        )

        # Discover an entity carrying PhysicsState, like the panels suite.
        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=\S+[^\n]*PhysicsState",
            60,
            "entity discovered",
            begin,
        )
        entity = None
        for match in re.finditer(
            re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=(\S+)[^\n]*", session.read_log()[begin:]
        ):
            if "PhysicsState" in match.group(0):
                entity = match.group(1)
                break
        assert entity, "no entity carrying PhysicsState in the dump"
        print(f"  [OK] entity {entity}")
        position = "pill_engine::common_components::Position"

        begin = len(session.read_log())
        session.send(f"select {entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf".*entity={re.escape(entity)}",
            30,
            "selected",
            begin,
        )
        begin = len(session.read_log())
        session.send("tab Inspector")
        session.wait(
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title="?Inspector"?',
            30,
            "Inspector tab",
            begin,
        )

        # Step 1: widget text. Field x's text is copied through the focused
        # field's own Ctrl+C, and pasted into field y with Ctrl+V.
        field_x = f"inspector.{entity}.{position}.x"
        field_y = f"inspector.{entity}.{position}.y"
        begin = len(session.read_log())
        session.send(f"activate {field_x}")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + rf'\s+name="?{re.escape(field_x)}"?',
            30,
            "field x focused",
            begin,
        )
        session.send("chord ctrl+a")
        session.send("type 424.5")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="set Position\.x"',
            60,
            "x typed",
            begin,
        )
        # The selection collapsed while typing, so select it again before
        # the field's own Ctrl+C sees anything.
        begin = len(session.read_log())
        session.send("chord ctrl+a")
        session.send("chord ctrl+c")
        session.wait(
            re.escape(CLIPBOARD_SET_TEXT_TOKEN) + r"\s+len=5", 30, "text copied", begin
        )

        begin = len(session.read_log())
        session.send(f"activate {field_y}")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + rf'\s+name="?{re.escape(field_y)}"?',
            30,
            "field y focused",
            begin,
        )
        session.send("chord ctrl+a")
        session.send("chord ctrl+v")
        session.wait(
            re.escape(CLIPBOARD_READ_TEXT_TOKEN) + r"\s+len=5", 30, "clipboard read", begin
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="set Position\.y"',
            60,
            "y pasted",
            begin,
        )
        assert dump_field(session, entity, "Position", "y") == "F32(424.5)", (
            "the paste landed as text"
        )
        print("  [OK] pasted text landed in the field")
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="set Position\.y"',
            30,
            "paste undone",
            begin,
        )

        # Step 2: the entity. Copy offers the selection; paste creates a new
        # entity carrying the same components - one undo step.
        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITIES_COUNT_TOKEN) + r"\s+count=\d+", 30, "count before", begin
        )
        before = entity_count(session)
        # The entity copy below must not see the Inspector field from step 1:
        # clear its focus first.
        blur_focused_field(session)
        begin = len(session.read_log())
        session.send("command editor.copy")
        session.wait(
            re.escape(CLIPBOARD_COPIED_TOKEN) + r'\s+kind="?entity"?', 30, "entity copied", begin
        )
        session.wait_token(CLIPBOARD_WROTE_OBJECT_TOKEN, 30, "payload written", begin)
        begin = len(session.read_log())
        session.send("command editor.paste")
        session.wait_token(CLIPBOARD_READ_REQUESTED_TOKEN, 30, "read requested", begin)
        session.wait(
            re.escape(CLIPBOARD_PASTE_ENTITY_TOKEN) + r"\s+components=\d+",
            60,
            "entity pasted",
            begin,
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="paste entity"',
            30,
            "paste recorded",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITIES_COUNT_TOKEN) + r"\s+count=\d+", 30, "count after", begin
        )
        after = entity_count(session)
        assert after == before + 1, f"paste must add one entity ({before} -> {after})"
        print(f"  [OK] entity pasted ({before} -> {after})")
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="paste entity"',
            30,
            "paste undone",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITIES_COUNT_TOKEN) + r"\s+count=\d+", 30, "count restored", begin
        )
        assert entity_count(session) == before, "undo must remove the pasted entity"
        print("  [OK] undo removed the pasted entity")

        # Step 3: one component. The Inspector's Copy button offers it; the
        # paste writes its values onto the other entity - one undo step.
        begin = len(session.read_log())
        session.send(f"select {entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf".*entity={re.escape(entity)}",
            30,
            "re-selected",
            begin,
        )
        begin = len(session.read_log())
        # The button offers the component, but the field from step 1 may still
        # hold the focus; clear it so the offer cannot read as a field copy.
        blur_focused_field(session)
        session.send("activate Copy Position")
        session.wait(
            re.escape(CLIPBOARD_COPIED_TOKEN) + r'\s+kind="?component"?',
            30,
            "component copied",
            begin,
        )
        copied = dump_field(session, entity, "Position", "x")
        print(f"  [OK] copied Position (x = {copied})")
        # Find a second entity carrying Position to paste onto.
        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITIES_COUNT_TOKEN) + r"\s+count=\d+", 30, "entity list", begin
        )
        other = None
        for match in re.finditer(
            re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=(\S+)[^\n]*", session.read_log()[begin:]
        ):
            if match.group(1) != entity and "Position" in match.group(0):
                other = match.group(1)
                break
        assert other, "no second entity carrying Position"
        print(f"  [OK] target entity {other}")
        begin = len(session.read_log())
        session.send(f"select {other}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf".*entity={re.escape(other)}",
            30,
            "other selected",
            begin,
        )
        original = dump_field(session, other, "Position", "x")
        begin = len(session.read_log())
        session.send("command editor.paste")
        session.wait_token(CLIPBOARD_READ_REQUESTED_TOKEN, 30, "read requested", begin)
        session.wait_token(CLIPBOARD_PASTE_COMPONENT_TOKEN, 60, "component pasted", begin)
        assert dump_field(session, other, "Position", "x") == copied, (
            "the component's values must land"
        )
        print(f"  [OK] component pasted ({original} -> {copied})")
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="paste Position"',
            30,
            "component paste undone",
            begin,
        )
        assert dump_field(session, other, "Position", "x") == original, (
            "undo must restore the values"
        )

        # Step 4: a file dropped on the Assets panel is copied into the
        # project's resources, imported by the asset pipeline, and undo
        # removes the copy. The Assets tab must be the selected tab of its
        # set, or the panel has no placement to drop onto.
        begin = len(session.read_log())
        session.send("tab Assets")
        session.wait(
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title="?Assets"?',
            30,
            "Assets tab",
            begin,
        )
        DROP_SOURCE.parent.mkdir(parents=True, exist_ok=True)
        # A real 1x1 PNG: the importer writes metadata only after the source
        # decodes, so a stub that merely looks like a PNG would never import.
        DROP_SOURCE.write_bytes(
            base64.b64decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"
                "AAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
            )
        )
        if DROP_COPY.exists():
            DROP_COPY.unlink()
        # Let the shell publish the panel's rectangle to the executable
        # (the scripted verbs aim there).
        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITIES_COUNT_TOKEN) + r"\s+count=\d+", 30, "paced", begin
        )
        # One beat after the paced count, so the focus settles before the hover.
        time.sleep(1.0)
        begin = len(session.read_log())
        session.send(f"hover-file {DROP_SOURCE}")
        session.wait_token(DROP_PREVIEW_TOKEN, 30, "drop preview shown", begin)
        begin = len(session.read_log())
        session.send(f"drop-file {DROP_SOURCE}")
        session.wait_token(IMPORT_QUEUED_TOKEN, 30, "import queued", begin)
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="import file"',
            30,
            "import recorded",
            begin,
        )
        deadline = time.monotonic() + 30
        while not DROP_COPY.exists() and time.monotonic() < deadline:
            # Poll until the drop's copy lands in the asset root.
            time.sleep(0.25)
        assert DROP_COPY.exists(), f"the drop must copy into {ASSET_ROOT}"
        session.wait_token(IMPORTED_ASSET_TOKEN, 60, "asset imported", begin)
        metadata = DROP_COPY.with_name(DROP_COPY.name + ".meta")
        deadline = time.monotonic() + 30
        while not metadata.exists() and time.monotonic() < deadline:
            # Poll until the import writes its metadata sidecar.
            time.sleep(0.25)
        assert metadata.exists(), "the import must write metadata"
        print("  [OK] dropped file imported with metadata")
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="import file"',
            30,
            "import undone",
            begin,
        )
        deadline = time.monotonic() + 30
        while DROP_COPY.exists() and time.monotonic() < deadline:
            # Poll until the undo removes the copied file.
            time.sleep(0.25)
        assert not DROP_COPY.exists(), "undo must remove the copied file"
        print("  [OK] undo removed the imported file")

        # Step 4b: several files dropped together share one undo step, and a
        # type no imported asset type claims still copies - without metadata.
        multi_sources = [
            REPOSITORY_ROOT / "local" / "temp" / name
            for name in ("s18_multi1.png", "s18_multi2.png")
        ]
        multi_copies = [ASSET_ROOT / source.name for source in multi_sources]
        for source, copy in zip(multi_sources, multi_copies):
            source.write_bytes(DROP_SOURCE.read_bytes())
            copy.unlink(missing_ok=True)
        begin = len(session.read_log())
        # One write, so a single poll hands the editor both at once.
        session.send_many([f"drop-file {source}" for source in multi_sources])
        session.wait(
            re.escape(IMPORT_QUEUED_TOKEN) + r"\s+count=2", 30, "multi import queued", begin
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="import 2 files"',
            30,
            "multi import recorded",
            begin,
        )
        for copy in multi_copies:
            deadline = time.monotonic() + 60
            while not copy.exists() and time.monotonic() < deadline:
                # Poll until each multi-drop copy lands.
                time.sleep(0.25)
            assert copy.exists(), f"the multi drop must copy {copy.name}"
        session.wait(
            re.escape(IMPORTED_ASSET_TOKEN) + r'\s+asset="s18_multi1\.png"',
            60,
            "multi import 1",
            begin,
        )
        session.wait(
            re.escape(IMPORTED_ASSET_TOKEN) + r'\s+asset="s18_multi2\.png"',
            60,
            "multi import 2",
            begin,
        )
        print("  [OK] two files imported in one step")
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="import 2 files"',
            30,
            "multi import undone",
            begin,
        )
        for copy in multi_copies:
            deadline = time.monotonic() + 30
            while copy.exists() and time.monotonic() < deadline:
                # Poll until the undo removes each copy.
                time.sleep(0.25)
            assert not copy.exists(), f"undo must remove {copy.name}"
        text_source = REPOSITORY_ROOT / "local" / "temp" / "s18_note.txt"
        text_copy = ASSET_ROOT / text_source.name
        text_source.write_text("not an asset\n", encoding="utf-8")
        text_copy.unlink(missing_ok=True)
        begin = len(session.read_log())
        session.send(f"drop-file {text_source}")
        session.wait(
            re.escape(IMPORT_QUEUED_TOKEN) + r"\s+count=1", 30, "text drop queued", begin
        )
        deadline = time.monotonic() + 30
        while not text_copy.exists() and time.monotonic() < deadline:
            # Poll until the unsupported type's copy lands.
            time.sleep(0.25)
        assert text_copy.exists(), "an unsupported type still copies into the asset root"
        # No imported type claims .txt, so no metadata may appear. The pause
        # gives a wrong import time to show up before the assertion.
        time.sleep(2.0)
        assert not text_copy.with_name(text_copy.name + ".meta").exists(), (
            "no metadata for an unsupported type"
        )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="import file"',
            30,
            "text drop undone",
            begin,
        )
        deadline = time.monotonic() + 30
        while text_copy.exists() and time.monotonic() < deadline:
            # Poll until the undo removes the unsupported copy.
            time.sleep(0.25)
        assert not text_copy.exists(), "undo must remove the unsupported type's copy"
        print("  [OK] unsupported type copied without metadata, undone")

        # Step 5: dragging a file out of the window starts OLE's modal drag
        # loop; Explorer itself cannot be scripted, so this smoke
        # run starts the drag and ends it with an injected mouse move (the
        # gesture's own end, since no button is down). The manual checklist
        # (D26) covers the actual copy into Explorer.
        begin = len(session.read_log())
        session.send(f"drag-out {DROP_SOURCE}")
        session.wait_token(EDITOR_DRAG_OUT_BEGAN_TOKEN, 30, "drag out began", begin)
        # The drag loop runs on the event loop thread, so give it a moment
        # to take over before the injected input.
        time.sleep(0.5)
        end_the_ole_drag(session.process.pid)
        session.wait_token(EDITOR_DRAG_OUT_ENDED_TOKEN, 30, "drag out ended", begin)
        print("  [OK] drag out started and ended without hanging the editor")

        # Step 6: re-copy the entity - the phases since overwrote the
        # system clipboard with a component - then a graceful quit with the
        # clean exit code. The copy is what the second instance pastes.
        begin = len(session.read_log())
        session.send(f"select {entity}")
        session.wait(
            re.escape(SELECTION_CHANGED_TOKEN) + rf".*entity={re.escape(entity)}",
            30,
            "re-selected",
            begin,
        )
        blur_focused_field(session)
        session.send("command editor.copy")
        session.wait(
            re.escape(CLIPBOARD_COPIED_TOKEN) + r'\s+kind="?entity"?',
            30,
            "entity re-copied",
            begin,
        )
        status = session.quit_and_wait(timeout=120)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in session.read_log().lower(), "panic in the log"
        print("  [OK] clean exit, code 0")

        # Step 7: the system clipboard crosses processes (D28): a second,
        # fresh editor on the same project pastes the copied entity with
        # identical component values. Sequential instances only - two at
        # once would fight over the project's files.
        second = EditorSession("clipboard", session="second", test_seconds=900)
        second.start()
        second.wait(
            re.escape(EDITOR_PANEL_REGISTRY_TOKEN) + r"\s+panels=\d+",
            300,
            "second editor ready",
        )
        begin = len(second.read_log())
        second.send("command editor.pause_toggle")
        second.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "second engine paused",
            begin,
        )
        before = entity_ids(second)
        begin = len(second.read_log())
        second.send("command editor.paste")
        second.wait_token(CLIPBOARD_READ_REQUESTED_TOKEN, 30, "second read requested", begin)
        second.wait(
            re.escape(CLIPBOARD_PASTE_ENTITY_TOKEN) + r"\s+components=(\d+)",
            30,
            "second paste",
            begin,
        )
        second.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="paste entity"',
            30,
            "second paste recorded",
            begin,
        )
        after = entity_ids(second)
        created = after - before
        assert len(created) == 1, f"one entity must appear, got {sorted(created)}"
        new_entity = created.pop()
        copied = dump_field(second, new_entity, position, "x")
        assert copied == "F32(424.5)", f"the pasted entity must carry the copied x, got {copied}"
        print(f"  [OK] second instance pasted {new_entity} with the copied value ({copied})")
        begin = len(second.read_log())
        second.send("command editor.undo")
        second.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="paste entity"',
            30,
            "second paste undone",
            begin,
        )
        assert entity_ids(second) == before, "undo must remove the pasted entity"
        print("  [OK] second instance's paste undone")
        status = second.quit_and_wait(timeout=120)
        assert status == 0, f"second editor exited {status}"
        assert "panic" not in second.read_log().lower(), "panic in the log"
        print("  [OK] second editor clean exit, code 0")
        print("PASS: clipboard text, entities, components, file drops, drag out and cross-instance paste")
        return 0
    finally:
        session.close()
        if second is not None:
            second.close()
        # The drop phases' artifacts never outlive the run (the watch would
        # otherwise keep orphan metadata files around).
        leftovers = [DROP_COPY, DROP_COPY.with_name(DROP_COPY.name + ".meta")]
        leftovers += [
            ASSET_ROOT / name
            for name in ("s18_multi1.png", "s18_multi2.png", "s18_note.txt")
        ]
        leftovers += [
            ASSET_ROOT / name for name in ("s18_multi1.png.meta", "s18_multi2.png.meta")
        ]
        for leftover in leftovers:
            leftover.unlink(missing_ok=True)
        try:
            ASSET_ROOT.rmdir()
        except OSError:
            pass  # not empty (or never existed): leave it be


if __name__ == "__main__":
    sys.exit(main())

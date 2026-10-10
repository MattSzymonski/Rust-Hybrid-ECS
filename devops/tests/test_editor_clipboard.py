"""End-to-end acceptance for the clipboard (W-0004 Stage 18).

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
  under its own LOCALAPPDATA sandbox.

USAGE
  python devops/tests/test_editor_clipboard.py

--- SCRIPT ---
"""

import base64
import ctypes
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULES = ROOT / "modules"
BINARY = MODULES / "target" / "debug" / "pill_editor_native.exe"
VALIDATION = ROOT / "local" / "editor-validation"
LOG = VALIDATION / "editor-clipboard.log"
COMMANDS = VALIDATION / "commands-stage18.txt"
APPDATA = VALIDATION / "stage18_appdata"
# The drop phase's fixtures: a source file outside the project and the
# project's asset directory the import copies into.
DROP_SOURCE = ROOT / "local" / "temp" / "s18_drop.png"
ASSET_ROOT = ROOT / "examples" / "project_rs" / "res"
DROP_COPY = ASSET_ROOT / "s18_drop.png"


def read_log() -> str:
    """The editor log so far, tolerant of a file the writer holds open."""
    try:
        return LOG.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def wait_token(process, pattern: str, timeout: float, label: str, start: int = 0) -> int:
    """Wait for `pattern` after byte offset `start`; returns the new length."""
    deadline = time.monotonic() + timeout
    expression = re.compile(pattern)
    while time.monotonic() < deadline:
        text = read_log()
        if expression.search(text[start:]):
            print(f"  [OK] {label}")
            return len(text)
        if process.poll() is not None:
            raise RuntimeError(
                f"{label}: the editor exited {process.returncode}\n" + text[-4000:]
            )
        time.sleep(0.35)
    raise TimeoutError(f"{label}: timeout waiting for {pattern}\n" + read_log()[-4000:])


def find_in_log(pattern: str, start: int = 0) -> re.Match:
    """The first match of `pattern` in the log from `start`, now."""
    match = re.compile(pattern).search(read_log()[start:])
    if not match:
        raise RuntimeError(f"no match for {pattern}\n" + read_log()[-3000:])
    return match


def append(line: str) -> None:
    """Appends one scripted command, flushed, so the editor sees it whole."""
    with COMMANDS.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def append_many(lines: list) -> None:
    """Appends several scripted commands in one write.

    The channel polls once per frame, so a single write hands them over in
    the same frame - needed when the meaning depends on them arriving
    together (a multi-file drop).
    """
    with COMMANDS.open("a", encoding="utf-8") as file:
        file.write("".join(line + "\n" for line in lines))


def entity_count() -> int:
    """The live entity count the latest `dump entities` reported."""
    matches = re.findall(r"\[test\] entities\s+count=(\d+)", read_log())
    if not matches:
        raise RuntimeError("no entities dump in the log\n" + read_log()[-2000:])
    return int(matches[-1])


def append_to(commands: Path, line: str) -> None:
    """Appends one scripted command to an instance's channel, flushed."""
    with commands.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def entity_ids(process, commands: Path) -> set:
    """The live entity ids this instance's fresh `dump entities` reports."""
    begin = len(read_log())
    append_to(commands, "dump entities")
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        ids = set(re.findall(r"\[test\] entity\s+entity=(\S+?)\s", read_log()[begin:]))
        if ids:
            return ids
        if process.poll() is not None:
            raise RuntimeError(
                f"the editor exited {process.returncode} during a dump\n" + read_log()[-3000:]
            )
        time.sleep(0.35)
    raise RuntimeError("the entity dump never answered\n" + read_log()[-3000:])


def dump_field(process, commands: Path, entity: str, component: str, field: str) -> str:
    """The value token one `dump field` reports, for any instance."""
    begin = len(read_log())
    append_to(commands, f"dump field {entity} {component} {field}")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        match = re.search(r"value=([A-Za-z0-9]+)\(([^)]+)\)", read_log()[begin:])
        if match:
            return f"{match.group(1)}({match.group(2)})"
        time.sleep(0.35)
    raise RuntimeError("the field dump never answered\n" + read_log()[-3000:])


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
    if os.name != "nt":
        print("SKIP: the native editor is Windows-only until Stage 24")
        return 0
    if not BINARY.is_file():
        raise SystemExit(
            f"editor binary not found at {BINARY}; build it with "
            "`cargo build -p pill_editor_native` in modules/ first"
        )
    subprocess.run(
        ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    VALIDATION.mkdir(parents=True, exist_ok=True)
    shutil.rmtree(APPDATA, ignore_errors=True)
    COMMANDS.write_text("# stage 18 clipboard suite\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/project_rs"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "900"
    environment["LOCALAPPDATA"] = str(APPDATA)
    log = LOG.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(BINARY)],
        cwd=str(MODULES),
        env=environment,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    log.close()
    second: subprocess.Popen | None = None
    try:
        wait_token(process, r"\[editor\] pill_editor_shell_editor loaded", 300, "shell loaded")
        wait_token(process, r"\[editor\] window created", 300, "editor started")
        wait_token(process, r"\[editor\] command registry ready", 120, "registry ready")
        wait_token(process, r"\[editor\] panel registry ready\s+panels=\d+", 120, "panels ready")

        # The project rewrites component fields every tick; pause first.
        begin = len(read_log())
        append("command editor.pause_toggle")
        wait_token(
            process,
            r'command executed\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            begin,
        )

        # Discover an entity carrying PhysicsState, like the panels suite.
        begin = len(read_log())
        append("dump entities")
        wait_token(
            process,
            r"\[test\] entity\s+entity=\S+[^\n]*PhysicsState",
            60,
            "entity discovered",
            begin,
        )
        entity = None
        for match in re.finditer(r"\[test\] entity\s+entity=(\S+)[^\n]*", read_log()[begin:]):
            if "PhysicsState" in match.group(0):
                entity = match.group(1)
                break
        assert entity, "no entity carrying PhysicsState in the dump"
        print(f"  [OK] entity {entity}")
        position = "pill_engine::common_components::Position"

        begin = len(read_log())
        append(f"select {entity}")
        wait_token(process, rf"selection changed.*entity={re.escape(entity)}", 30, "selected", begin)
        begin = len(read_log())
        append("tab Inspector")
        wait_token(process, r'tab selected\s+title="?Inspector"?', 30, "Inspector tab", begin)

        # Step 1: widget text. Field x's text is copied through the focused
        # field's own Ctrl+C, and pasted into field y with Ctrl+V.
        field_x = f"inspector.{entity}.{position}.x"
        field_y = f"inspector.{entity}.{position}.y"
        begin = len(read_log())
        append(f"activate {field_x}")
        wait_token(
            process, rf"widget activated\s+name=\"?{re.escape(field_x)}\"?", 30, "field x focused", begin
        )
        append("chord ctrl+a")
        append("type 424.5")
        wait_token(process, r'history push\s+label="set Position\.x"', 60, "x typed", begin)
        # The selection collapsed while typing, so select it again before
        # the field's own Ctrl+C sees anything.
        begin = len(read_log())
        append("chord ctrl+a")
        append("chord ctrl+c")
        wait_token(process, r"clipboard set text\s+len=5", 30, "text copied", begin)

        begin = len(read_log())
        append(f"activate {field_y}")
        wait_token(
            process, rf"widget activated\s+name=\"?{re.escape(field_y)}\"?", 30, "field y focused", begin
        )
        append("chord ctrl+a")
        append("chord ctrl+v")
        wait_token(process, r"clipboard read text\s+len=5", 30, "clipboard read", begin)
        wait_token(process, r'history push\s+label="set Position\.y"', 60, "y pasted", begin)
        assert dump_field(process, COMMANDS, entity, "Position", "y") == "F32(424.5)", "the paste landed as text"
        print("  [OK] pasted text landed in the field")
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="set Position\.y"', 30, "paste undone", begin)

        # Step 2: the entity. Copy offers the selection; paste creates a new
        # entity carrying the same components - one undo step.
        begin = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entities\s+count=\d+", 30, "count before", begin)
        before = entity_count()
        begin = len(read_log())
        append("command editor.copy")
        wait_token(process, r"clipboard copied\s+kind=\"?entity\"?", 30, "entity copied", begin)
        wait_token(process, r"clipboard wrote object", 30, "payload written", begin)
        begin = len(read_log())
        append("command editor.paste")
        wait_token(process, r"clipboard read requested", 30, "read requested", begin)
        wait_token(process, r"clipboard paste entity\s+components=\d+", 60, "entity pasted", begin)
        wait_token(process, r'history push\s+label="paste entity"', 30, "paste recorded", begin)
        begin = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entities\s+count=\d+", 30, "count after", begin)
        after = entity_count()
        assert after == before + 1, f"paste must add one entity ({before} -> {after})"
        print(f"  [OK] entity pasted ({before} -> {after})")
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="paste entity"', 30, "paste undone", begin)
        begin = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entities\s+count=\d+", 30, "count restored", begin)
        assert entity_count() == before, "undo must remove the pasted entity"
        print("  [OK] undo removed the pasted entity")

        # Step 3: one component. The Inspector's Copy button offers it; the
        # paste writes its values onto the other entity - one undo step.
        begin = len(read_log())
        append(f"select {entity}")
        wait_token(process, rf"selection changed.*entity={re.escape(entity)}", 30, "re-selected", begin)
        begin = len(read_log())
        append("activate Copy Position")
        wait_token(process, r"clipboard copied\s+kind=\"?component\"?", 30, "component copied", begin)
        copied = dump_field(process, COMMANDS, entity, "Position", "x")
        print(f"  [OK] copied Position (x = {copied})")
        # Find a second entity carrying Position to paste onto.
        begin = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entities\s+count=\d+", 30, "entity list", begin)
        other = None
        for match in re.finditer(r"\[test\] entity\s+entity=(\S+)[^\n]*", read_log()[begin:]):
            if match.group(1) != entity and "Position" in match.group(0):
                other = match.group(1)
                break
        assert other, "no second entity carrying Position"
        print(f"  [OK] target entity {other}")
        begin = len(read_log())
        append(f"select {other}")
        wait_token(process, rf"selection changed.*entity={re.escape(other)}", 30, "other selected", begin)
        original = dump_field(process, COMMANDS, other, "Position", "x")
        begin = len(read_log())
        append("command editor.paste")
        wait_token(process, r"clipboard read requested", 30, "read requested", begin)
        wait_token(process, r"clipboard paste component", 60, "component pasted", begin)
        assert dump_field(process, COMMANDS, other, "Position", "x") == copied, "the component's values must land"
        print(f"  [OK] component pasted ({original} -> {copied})")
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="paste Position"', 30, "component paste undone", begin)
        assert dump_field(process, COMMANDS, other, "Position", "x") == original, "undo must restore the values"

        # Step 4: a file dropped on the Assets panel is copied into the
        # project's resources, imported by the asset pipeline, and undo
        # removes the copy. The Assets tab must be the selected tab of its
        # set, or the panel has no placement to drop onto.
        begin = len(read_log())
        append("tab Assets")
        wait_token(process, r'tab selected\s+title="?Assets"?', 30, "Assets tab", begin)
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
        begin = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entities\s+count=\d+", 30, "paced", begin)
        time.sleep(1.0)
        begin = len(read_log())
        append(f"hover-file {DROP_SOURCE}")
        wait_token(process, r"drop preview shown", 30, "drop preview shown", begin)
        begin = len(read_log())
        append(f"drop-file {DROP_SOURCE}")
        wait_token(process, r"file drop import queued", 30, "import queued", begin)
        wait_token(process, r'history push\s+label="import file"', 30, "import recorded", begin)
        deadline = time.monotonic() + 30
        while not DROP_COPY.exists() and time.monotonic() < deadline:
            time.sleep(0.25)
        assert DROP_COPY.exists(), f"the drop must copy into {ASSET_ROOT}"
        wait_token(process, r"\[assets\] imported a new asset", 60, "asset imported", begin)
        metadata = DROP_COPY.with_name(DROP_COPY.name + ".meta")
        deadline = time.monotonic() + 30
        while not metadata.exists() and time.monotonic() < deadline:
            time.sleep(0.25)
        assert metadata.exists(), "the import must write metadata"
        print("  [OK] dropped file imported with metadata")
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="import file"', 30, "import undone", begin)
        deadline = time.monotonic() + 30
        while DROP_COPY.exists() and time.monotonic() < deadline:
            time.sleep(0.25)
        assert not DROP_COPY.exists(), "undo must remove the copied file"
        print("  [OK] undo removed the imported file")

        # Step 4b: several files dropped together share one undo step, and a
        # type no imported asset type claims still copies - without metadata.
        multi_sources = [ROOT / "local" / "temp" / name for name in ("s18_multi1.png", "s18_multi2.png")]
        multi_copies = [ASSET_ROOT / source.name for source in multi_sources]
        for source, copy in zip(multi_sources, multi_copies):
            source.write_bytes(DROP_SOURCE.read_bytes())
            copy.unlink(missing_ok=True)
        begin = len(read_log())
        # One write, so a single poll hands the editor both at once.
        append_many([f"drop-file {source}" for source in multi_sources])
        wait_token(process, r"file drop import queued\s+count=2", 30, "multi import queued", begin)
        wait_token(process, r'history push\s+label="import 2 files"', 30, "multi import recorded", begin)
        for copy in multi_copies:
            deadline = time.monotonic() + 60
            while not copy.exists() and time.monotonic() < deadline:
                time.sleep(0.25)
            assert copy.exists(), f"the multi drop must copy {copy.name}"
        wait_token(process, r'\[assets\] imported a new asset\s+asset="s18_multi1\.png"', 60, "multi import 1", begin)
        wait_token(process, r'\[assets\] imported a new asset\s+asset="s18_multi2\.png"', 60, "multi import 2", begin)
        print("  [OK] two files imported in one step")
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="import 2 files"', 30, "multi import undone", begin)
        for copy in multi_copies:
            deadline = time.monotonic() + 30
            while copy.exists() and time.monotonic() < deadline:
                time.sleep(0.25)
            assert not copy.exists(), f"undo must remove {copy.name}"
        text_source = ROOT / "local" / "temp" / "s18_note.txt"
        text_copy = ASSET_ROOT / text_source.name
        text_source.write_text("not an asset\n", encoding="utf-8")
        text_copy.unlink(missing_ok=True)
        begin = len(read_log())
        append(f"drop-file {text_source}")
        wait_token(process, r"file drop import queued\s+count=1", 30, "text drop queued", begin)
        deadline = time.monotonic() + 30
        while not text_copy.exists() and time.monotonic() < deadline:
            time.sleep(0.25)
        assert text_copy.exists(), "an unsupported type still copies into the asset root"
        # No imported type claims .txt, so no metadata may appear. The pause
        # gives a wrong import time to show up before the assertion.
        time.sleep(2.0)
        assert not text_copy.with_name(text_copy.name + ".meta").exists(), "no metadata for an unsupported type"
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="import file"', 30, "text drop undone", begin)
        deadline = time.monotonic() + 30
        while text_copy.exists() and time.monotonic() < deadline:
            time.sleep(0.25)
        assert not text_copy.exists(), "undo must remove the unsupported type's copy"
        print("  [OK] unsupported type copied without metadata, undone")

        # Step 5: dragging a file out of the window starts OLE's modal drag
        # loop (Stage 18); Explorer itself cannot be scripted, so this smoke
        # run starts the drag and ends it with an injected mouse move (the
        # gesture's own end, since no button is down). The manual checklist
        # (D26) covers the actual copy into Explorer.
        begin = len(read_log())
        append(f"drag-out {DROP_SOURCE}")
        wait_token(process, r"\[editor\] drag out began", 30, "drag out began", begin)
        # The drag loop runs on the event loop thread, so give it a moment
        # to take over before the injected input.
        time.sleep(0.5)
        end_the_ole_drag(process.pid)
        wait_token(process, r"\[editor\] drag out ended", 30, "drag out ended", begin)
        print("  [OK] drag out started and ended without hanging the editor")

        # Step 6: re-copy the entity - the phases since overwrote the
        # system clipboard with a component - then a graceful quit with the
        # clean exit code. The copy is what the second instance pastes.
        begin = len(read_log())
        append(f"select {entity}")
        wait_token(
            process, rf"selection changed.*entity={re.escape(entity)}", 30, "re-selected", begin
        )
        append("command editor.copy")
        wait_token(process, r'clipboard copied\s+kind="entity"', 30, "entity re-copied", begin)
        append("quit")
        wait_token(process, r"\[editor\] test channel: quit requested", 30, "quit command")
        process.wait(timeout=120)
        assert process.returncode == 0, f"editor exited {process.returncode} on the test quit"
        assert "panic" not in read_log().lower(), "panic in the log"
        print("  [OK] clean exit, code 0")

        # Step 7: the system clipboard crosses processes (D28): a second,
        # fresh editor on the same project pastes the copied entity with
        # identical component values. Sequential instances only - two at
        # once would fight over the project's files.
        second_commands = COMMANDS.with_name("commands-stage18b.txt")
        second_commands.write_text("# stage 18 clipboard suite, second instance\n", encoding="utf-8")
        second_environment = dict(environment)
        second_environment["PILL_EDITOR_TEST_COMMANDS"] = str(second_commands)
        second_log = LOG.open("a", encoding="utf-8")
        second = subprocess.Popen(
            [str(BINARY)],
            cwd=str(MODULES),
            env=second_environment,
            stdout=second_log,
            stderr=subprocess.STDOUT,
        )
        second_log.close()
        begin = len(read_log())
        wait_token(second, r"\[editor\] panel registry ready\s+panels=\d+", 300, "second editor ready", begin)

        def append_second(line: str) -> None:
            """Append to the second instance's scripted channel."""
            append_to(second_commands, line)

        begin = len(read_log())
        append_second("command editor.pause_toggle")
        wait_token(
            second,
            r'command executed\s+id="?editor\.pause_toggle"?',
            30,
            "second engine paused",
            begin,
        )
        before = entity_ids(second, second_commands)
        begin = len(read_log())
        append_second("command editor.paste")
        wait_token(second, r"clipboard read requested", 30, "second read requested", begin)
        wait_token(second, r"clipboard paste entity\s+components=(\d+)", 30, "second paste", begin)
        wait_token(second, r'history push\s+label="paste entity"', 30, "second paste recorded", begin)
        after = entity_ids(second, second_commands)
        created = after - before
        assert len(created) == 1, f"one entity must appear, got {sorted(created)}"
        new_entity = created.pop()
        copied = dump_field(second, second_commands, new_entity, position, "x")
        assert copied == "F32(424.5)", f"the pasted entity must carry the copied x, got {copied}"
        print(f"  [OK] second instance pasted {new_entity} with the copied value ({copied})")
        begin = len(read_log())
        append_second("command editor.undo")
        wait_token(second, r'history undo\s+label="paste entity"', 30, "second paste undone", begin)
        assert entity_ids(second, second_commands) == before, "undo must remove the pasted entity"
        print("  [OK] second instance's paste undone")
        append_second("quit")
        wait_token(second, r"\[editor\] test channel: quit requested", 30, "second quit command")
        second.wait(timeout=120)
        assert second.returncode == 0, f"second editor exited {second.returncode}"
        assert "panic" not in read_log().lower(), "panic in the log"
        print("  [OK] second editor clean exit, code 0")
        print("PASS: clipboard text, entities, components, file drops, drag out and cross-instance paste")
        return 0
    finally:
        for child in [process, second]:
            if child is not None and child.poll() is None:
                child.kill()
                child.wait(timeout=30)
        # The drop phases' artifacts never outlive the run (the watch would
        # otherwise keep orphan metadata files around).
        leftovers = [DROP_COPY, DROP_COPY.with_name(DROP_COPY.name + ".meta")]
        leftovers += [ASSET_ROOT / name for name in ("s18_multi1.png", "s18_multi2.png", "s18_note.txt")]
        leftovers += [ASSET_ROOT / name for name in ("s18_multi1.png.meta", "s18_multi2.png.meta")]
        for leftover in leftovers:
            leftover.unlink(missing_ok=True)
        try:
            ASSET_ROOT.rmdir()
        except OSError:
            pass  # not empty (or never existed): leave it be


if __name__ == "__main__":
    sys.exit(main())

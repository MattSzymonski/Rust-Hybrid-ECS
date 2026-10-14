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

EXAMPLE USAGE
  python devops/tests/test_editor_selection.py

--- SCRIPT ---
"""

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
LOG = VALIDATION / "editor-selection.log"
COMMANDS = VALIDATION / "commands-selection.txt"
APPDATA = VALIDATION / "selection_appdata"


def read_log() -> str:
    """The editor log so far, tolerant of a file the writer holds open."""
    try:
        return LOG.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def wait_token(
    process: subprocess.Popen, pattern: str, timeout: float, label: str, start: int = 0
) -> int:
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
        time.sleep(0.5)
    raise TimeoutError(f"{label}: timeout waiting for {pattern}\n" + read_log()[-4000:])


def wait_either(
    process: subprocess.Popen, patterns: "list[str]", timeout: float, start: int
) -> "tuple[str, int]":
    """Wait for the first of `patterns`; returns the one found and the length."""
    deadline = time.monotonic() + timeout
    expressions = [(pattern, re.compile(pattern)) for pattern in patterns]
    while time.monotonic() < deadline:
        text = read_log()
        for pattern, expression in expressions:
            if expression.search(text[start:]):
                return pattern, len(text)
        if process.poll() is not None:
            raise RuntimeError(
                f"the editor exited {process.returncode}\n" + text[-4000:]
            )
        time.sleep(0.5)
    raise TimeoutError(
        f"timeout waiting for any of {patterns}\n" + read_log()[-4000:]
    )


def append(line: str) -> None:
    """Appends one scripted command, flushed, so the editor sees it whole."""
    with COMMANDS.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def scene_rect() -> "tuple[float, float, float, float]":
    """The Scene tab's dock rectangle from the shell's placement log."""
    match = re.search(
        r'\[editor\] panel placement\s+kind="Scene"\s+x=([\d.]+)\s+y=([\d.]+)\s+'
        r'width=([\d.]+)\s+height=([\d.]+)',
        read_log(),
    )
    if not match:
        raise RuntimeError("no Scene placement in the log:\n" + read_log()[-3000:])
    return tuple(float(value) for value in match.groups())


def dump_array(
    process: subprocess.Popen, entity: str, component: str, field: str
) -> "list[float]":
    """The F32 elements one `dump field` reports for an array field, in order."""
    begin = len(read_log())
    append(f"dump field {entity} {component} {field}")
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        text = read_log()[begin:]
        match = re.search(r"value=Array\(\[([^\]]+)\]\)", text)
        if match:
            return [float(value) for value in re.findall(r"F32\(([-\d.eE+]+)\)", match.group(1))]
        if "[test] field unreadable" in text:
            raise RuntimeError(f"{entity} has no readable {component}.{field}\n" + text[-2000:])
        if process.poll() is not None:
            raise RuntimeError(
                f"the editor exited {process.returncode} during a dump\n" + text[-3000:]
            )
        time.sleep(0.4)
    raise RuntimeError("the field dump never answered\n" + read_log()[-3000:])


def main() -> int:
    if os.name != "nt":
        print("SKIP: the native editor suites need Windows")
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
    COMMANDS.write_text("# selection suite\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/italian_brainrot"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "600"
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
    try:
        wait_token(process, r"\[editor\] pill_editor_shell_editor loaded", 300, "shell loaded")
        wait_token(process, r"\[editor\] window created", 300, "editor started")
        wait_token(process, r"\[editor\] panel placement\s+kind=\"Scene\"", 120, "scene docked")

        # Step 1: the scene view's rectangle, in logical client pixels - the
        # space the scripted clicks use.
        x, y, width, height = scene_rect()
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
            begin = len(read_log())
            append(f"click {click_x:.0f} {click_y:.0f}")
            found, _ = wait_either(
                process,
                [
                    r"\[editor\] scene pick entity\s+entity=(\S+)",
                    r"\[editor\] scene pick nothing",
                ],
                120,
                begin,
            )
            if found.startswith(r"\[editor\] scene pick entity"):
                match = re.search(
                    r"\[editor\] scene pick entity\s+entity=(\S+)", read_log()[begin:]
                )
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
        wait_token(
            process,
            rf"\[editor\] selection changed\s+entity={re.escape(picked_entity)}",
            60,
            "the pick moved the shared selection",
        )
        wait_token(
            process,
            rf"\[editor\] outline set\s+entity={re.escape(picked_entity)}",
            60,
            "the outline was published for it",
        )

        # Step 3: a corner of the view holds nothing, which clears both.
        begin = len(read_log())
        append(f"click {x + 6:.0f} {y + 6:.0f}")
        wait_token(
            process, r"\[editor\] scene pick nothing", 120, "the corner hits nothing", begin
        )
        wait_token(
            process, r"\[editor\] outline cleared", 60, "the outline cleared with it", begin
        )

        # Step 4: the gizmos. The corner click cleared the
        # selection, so the grid's entity is selected again and its gizmo
        # must appear where the handles are drawn.
        begin = len(read_log())
        append(f"select {picked_entity}")
        wait_token(
            process,
            rf"\[editor\] selection changed\s+entity={re.escape(picked_entity)}",
            60,
            "the picked entity is selected again",
            begin,
        )
        wait_token(
            process,
            rf"\[editor\] gizmo anchor\s+entity={re.escape(picked_entity)}"
            r'\s+mode="translate"\s+space="world"',
            120,
            "the translate handles are drawn for it",
            begin,
        )
        before = dump_array(process, picked_entity, "TransformComponent", "translation")
        print(f"  [OK] the entity's translation starts at {before}")

        # Step 5: one translate drag - press on the X handle's own pixel,
        # move 120 px right, release. The pick must answer a handle id, the
        # drag must write through the recorded path, and the whole drag must
        # be one undo step.
        begin = len(read_log())
        append("gizmo drag 0 120 0")
        wait_token(
            process, r"\[editor\] test channel: gizmo drag request", 30, "the scripted drag was read", begin
        )
        wait_token(
            process,
            r'\[editor\] gizmo drag press\s+axis="x"',
            60,
            "the press ray was aimed at the X handle",
            begin,
        )
        wait_token(
            process, r"\[editor\] scene pick handle\s+handle=\d+", 60, "the pick answered a handle", begin
        )
        wait_token(
            process,
            rf'\[editor\] gizmo drag started\s+mode="translate"\s+axis="x"'
            rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the drag started on the translate X handle",
            begin,
        )
        wait_token(
            process,
            r'\[editor\] gizmo drag ended\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),[^\n]*snapped=false",
            60,
            "the drag ended, unsnapped, with a value",
            begin,
        )
        wait_token(
            process, r'\[editor\] history push\s+label="gizmo translate"', 60, "the drag is one history step", begin
        )
        match = re.search(
            r'\[editor\] gizmo drag ended\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),",
            read_log()[begin:],
        )
        logged_x = float(match.group(1))
        after = dump_array(process, picked_entity, "TransformComponent", "translation")
        assert abs(after[0] - logged_x) < 1e-3, f"the world holds {after[0]}, the drag logged {logged_x}"
        assert abs(after[0] - before[0]) > 1e-4, "the drag must move the entity's X"
        print(f"  [OK] the drag moved X {before[0]:.3f} -> {after[0]:.3f} (logged)")

        # Step 6: one undo puts it back - the drag was a single step.
        begin = len(read_log())
        append("command editor.undo")
        wait_token(
            process, r'\[editor\] history undo\s+label="gizmo translate"', 60, "the undo step is the drag", begin
        )
        deadline = time.monotonic() + 30
        while abs(dump_array(process, picked_entity, "TransformComponent", "translation")[0] - before[0]) > 1e-4:
            if time.monotonic() > deadline:
                raise AssertionError("the undo never restored the translation")
        print(f"  [OK] one undo restored X to {before[0]:.3f}")

        # Step 7: snapping (the Tools command, persisted), then the same
        # handle dragged far enough that the 0.5 step is certain to bite.
        begin = len(read_log())
        append("command editor.tool_snap_toggle")
        wait_token(
            process, r"\[editor\] gizmo tool[^\n]*snap=Some\(true\)", 60, "snapping is on", begin
        )
        wait_token(
            process, r"\[editor\] gizmo preferences written[^\n]*snap=true", 60, "the choice is persisted", begin
        )
        begin = len(read_log())
        append("gizmo drag 0 200 0")
        wait_token(
            process,
            r'\[editor\] gizmo drag ended\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),[^\n]*snapped=true",
            60,
            "the snapped drag ended",
            begin,
        )
        match = re.search(
            r'\[editor\] gizmo drag ended\s+mode="translate"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),",
            read_log()[begin:],
        )
        snapped_x = float(match.group(1))
        moved = snapped_x - before[0]
        assert abs(moved) > 1e-4, "the snapped drag must still move"
        assert abs(moved / 0.5 - round(moved / 0.5)) < 1e-3, f"{moved} is not on the 0.5 step"
        after = dump_array(process, picked_entity, "TransformComponent", "translation")
        assert abs(after[0] - snapped_x) < 1e-3, f"the world holds {after[0]}, the drag logged {snapped_x}"
        print(f"  [OK] the snapped drag moved X to {snapped_x:.3f} (step {moved:.1f})")

        # Step 8: the rotate tool: the Y ring is grabbed on its own plane and
        # the drag writes the whole quaternion, still as one step.
        begin = len(read_log())
        append("command editor.tool_rotate")
        wait_token(
            process, r"\[editor\] gizmo tool[^\n]*mode=Some\(Rotate\)", 60, "the rotate tool is active", begin
        )
        begin = len(read_log())
        append("gizmo drag 1 100 0")
        wait_token(
            process,
            rf'\[editor\] gizmo drag started\s+mode="rotate"\s+axis="y"'
            rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the drag started on the Y ring",
            begin,
        )
        wait_token(
            process,
            r'\[editor\] gizmo drag ended\s+mode="rotate"\s+axis="y"\s+entity=\S+'
            r"\s+value=\[[-\d.eE+, ]+\]",
            60,
            "the ring drag ended with the quaternion",
            begin,
        )
        wait_token(
            process, r'\[editor\] history push\s+label="gizmo rotate"', 60, "the ring drag is one history step", begin
        )
        print("  [OK] the Y ring drag wrote the quaternion as one step")

        # Step 9: the scale tool, through its X handle; the written value is
        # the first element of the logged array.
        begin = len(read_log())
        append("command editor.tool_scale")
        wait_token(
            process, r"\[editor\] gizmo tool[^\n]*mode=Some\(Scale\)", 60, "the scale tool is active", begin
        )
        begin = len(read_log())
        append("gizmo drag 0 120 0")
        wait_token(
            process,
            rf'\[editor\] gizmo drag started\s+mode="scale"\s+axis="x"'
            rf"\s+entity={re.escape(picked_entity)}",
            60,
            "the drag started on the scale X handle",
            begin,
        )
        wait_token(
            process,
            r'\[editor\] gizmo drag ended\s+mode="scale"\s+axis="x"\s+entity=\S+'
            r"\s+value=\[([-\d.eE+]+),",
            60,
            "the scale drag ended with a value",
            begin,
        )
        wait_token(
            process, r'\[editor\] history push\s+label="gizmo scale"', 60, "the scale drag is one history step", begin
        )
        before_scale = dump_array(process, picked_entity, "TransformComponent", "scale")
        print(f"  [OK] the scale drag wrote it as one step (scale now {before_scale})")

        # Leave the tool as it was found, and exit cleanly.
        begin = len(read_log())
        append("command editor.tool_translate")
        wait_token(
            process, r"\[editor\] gizmo tool[^\n]*mode=Some\(Translate\)", 60, "the translate tool is back", begin
        )

        # Step 10: a clean exit.
        append("quit")
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.5)
        assert process.poll() == 0, f"the editor exited {process.poll()}"
        print("  [OK] the editor exited cleanly")
        return 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=30)


if __name__ == "__main__":
    sys.exit(main())

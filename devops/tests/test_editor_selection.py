"""End-to-end acceptance for scene selection (W-0004 Stage 20, selection half).

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/italian_brainrot with its master-renderer scene

DESCRIPTION
  Starts the editor on the Italian Brainrot project and drives scene selection
  through the renderer's pick contract:

    1. the Scene tab's rectangle is read from the shell's placement log;
    2. scripted clicks are aimed at a 3x3 grid inside it: at least one of them
       must hit an entity (`scene pick entity`), move the shared selection
       (`selection changed`, the same token a Hierarchy click logs) and publish
       the outline (`outline set`);
    3. a click on a corner of the view hits nothing, which clears the selection
       (`scene pick nothing`, `outline cleared`);
    4. the editor exits cleanly.

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
COMMANDS = VALIDATION / "commands-stage20.txt"
APPDATA = VALIDATION / "stage20_appdata"


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
    COMMANDS.write_text("# stage 20 selection suite\n", encoding="utf-8")
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

        # Step 4: a clean exit.
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

"""End-to-end acceptance for the egui editor frontend.

PROTECTS: The egui editor: startup, shell module, dock rearrangement, patch and reload state survival.
TIER: editor
SETS: editor

Builds against an already-built `pill_editor_egui` binary, starts it on
`italian_brainrot` - a three-entity project, so the frames are fast and the
heartbeat waits are short - with the scripted input channel
(`PILL_EDITOR_TEST_COMMANDS`), and asserts on its log tokens: startup and the
scene surface, the egui shell module loading, both tabs reporting rows, a
dock rearrangement (an undocked tab) through the scripted channel, a live
patch of the shell that keeps its frame counter running, a structural edit
that reloads the shell and keeps the rearranged dock, a project reload that
keeps the editor UI, and a clean exit.

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
FRAME_HEARTBEAT_TOKEN = "[editor] frame heartbeat"
PROJECT_RELOAD_TOKEN = "hot reload complete"
EXTENSION_RELOAD_TOKEN = "extension reload processed"

# The rows heartbeat's runtime values, in the field order the shell logs them.
ROWS_PATTERN = (
    re.escape(EGUI_ROWS_TOKEN) + r"\s+frames=(\d+)\s+entities=(\d+)\s+systems=(\d+)"
)
DOCK_DUMP_PATTERN = re.escape(EGUI_DOCK_DUMP_TOKEN) + r"\s+surfaces=(\d+)"


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
    binary = arguments.binary.resolve() if arguments.binary else None
    if binary is None and not EGUI_EDITOR_BINARY.is_file():
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
    # The phases wait for two row heartbeats and a patch compile; the
    # executable's seconds deadline is a hang guard, kept comfortably above
    # the phases even on a slow machine.
    session = EditorSession(
        "egui",
        "main",
        project=arguments.project,
        test_seconds=240,
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
        wait_for_token(session, re.escape(EGUI_SHELL_LOADED_TOKEN), 180, "shell load")
        rows = wait_for_token(session, ROWS_PATTERN, 180, "rows heartbeat")
        match = re.search(r"entities=(\d+)\s+systems=(\d+)", rows)
        assert match and int(match.group(1)) > 0, f"no entities in the rows: {rows}"
        print(f"  [OK] shell loaded, {rows}")

        # Step 2: the dock rearranges - an undocked tab becomes a second
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

        # Step 3: a live patch of the shell keeps the frame counter running.
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

        # Step 4: a structural edit of the shell reloads it; the rearranged
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

        # Step 5: a project reload keeps the editor UI and its dock.
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

        # Step 6: a graceful quit, with the clean exit code.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in session.read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0")
    finally:
        session.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

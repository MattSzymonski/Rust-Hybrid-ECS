"""End-to-end acceptance for reload feedback.

PROTECTS: Reload feedback: failed reloads become Problems rows and toasts, and bad patches roll back.
TIER: editor
SETS: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor.exe
  - examples/italian_brainrot and its pill_dummy_color extension (the suite
    breaks and repairs that extension's only source file)

DESCRIPTION
  Starts the editor on the Italian Brainrot project and watches what the host
  reports when a reload goes wrong and then right:

    1. a deliberate type error appended to pill_dummy_color's source makes its
       reload fail: the runner logs the parsed problem (module, file, line,
       message), the shell logs the toast it showed and the Problems panel's
       revision, and the failed generation keeps running;
    2. the Problems tab shows the row, and `activate problems.open.0` opens it:
       the shell asks the executable, the executable hands the file and line to
       the configured command (`PILL_EDITOR_OPEN_COMMAND`, a recorder here),
       and the recorder proves the file:line it received;
    3. restoring the source clears both: the module reloads, the problem list
       for it drops to zero, and a reload toast arrives with its build time;
    4. patching a `panic!` into the shell module's update system - the editor's
       own UI, covered like any module (D33) - is contained, reported once, and
       after three consecutive panicking frames the host rolls the patch back:
       a rollback toast and a Problems row arrive, frames keep running on the
       previous code, and restoring the body patches in again;
    5. the editor exits cleanly.

  Everything is asserted on the editor's log tokens and the recorder file; the
  edited sources are restored when the session closes. The suite runs under
  its own LOCALAPPDATA sandbox, so the default workspace (with the Problems
  tab) is the one that loads.

USAGE
  python devops/tests/test_editor_reload_feedback.py

  The run's log, command file and open recorder land in their own folder
  under `local/editor-validation/reload-feedback-<run id>/`.

EXAMPLE USAGE
  python devops/tests/test_editor_reload_feedback.py

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
    EDITOR_COMMAND_REGISTRY_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_TAB_SELECTED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)
from core.paths import MODULES_ROOT  # noqa: E402

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
SHELL_FRAME_TOKEN = "[editor] shell frame"
PROBLEMS_UPDATED_TOKEN = "[editor] problems updated"
PROBLEM_ROW_TOKEN = "[editor] problem"
TOAST_SHOWN_TOKEN = "[editor] toast shown"
PROBLEMS_PANEL_TOKEN = "[editor] problems panel updated"
PROBLEMS_ROW_ACTIVATED_TOKEN = "[editor] problems row activated"
OPEN_FILE_TOKEN = "[editor] open file"
HOT_PREFIX_TOKEN = "[hot] "
ROLLBACK_TOKEN = "patch rolled back automatically after repeated panics"
EXTENSION_RELOADED_TOKEN = "extension hot reload complete"

EXTENSION_SOURCE = (
    MODULES_ROOT
    / "extensions"
    / "pill_dummy_color"
    / "pill_dummy_color"
    / "src"
    / "pill_dummy_color.rs"
)
EXTENSION_NAME = "pill_dummy_color"

# The shell module's update system: the suite patches a panic into its body and
# expects the host to roll the patch back. The shell is covered like any module
# (D33), and the editor must survive its own UI system panicking every frame.
SHELL_SOURCE = (
    MODULES_ROOT
    / "extensions"
    / "pill_editor_shell"
    / "pill_editor_shell_editor"
    / "src"
    / "pill_editor_shell_editor.rs"
)
SHELL_BODY_ANCHOR = (
    "    let (mut state, mut input, mut frame, editor_snapshot, logs, clipboard_answer) = view;"
)
SHELL_PANIC_LINE = '    panic!("reload feedback rollback probe");'

# A type error with a span on the line it is appended as, so the expected
# line number is the file's new last line and the parser must find it.
BROKEN_LINE = 'pub fn reload_feedback_broken() -> u32 { "not a number" }'


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession("reload-feedback")
    session.run_directory.mkdir(parents=True, exist_ok=True)
    open_record = session.run_directory / "open-record.txt"
    open_bat = session.run_directory / "open.bat"
    # The recorder the editor's open request reaches: it appends its arguments
    # (the `-g` switch and the file:line) to a file the suite reads back.
    open_bat.write_text(f'@echo %* >> "{open_record}"\n', encoding="utf-8")
    open_record.unlink(missing_ok=True)
    session.start(
        project="../examples/italian_brainrot",
        test_seconds=900,
        extra_env={"PILL_EDITOR_OPEN_COMMAND": f"cmd /C {open_bat}"},
    )
    with EXTENSION_SOURCE.open("r", encoding="utf-8", newline="") as file:
        original_source = file.read()
    with SHELL_SOURCE.open("r", encoding="utf-8", newline="") as file:
        original_shell = file.read()
    try:
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait_token(EDITOR_COMMAND_REGISTRY_TOKEN, 120, "registry ready")
        # A paused, idle editor sleeps at its heartbeat rate and paints no
        # frames, so the engine is started ticking before the frame waits
        # (the editor can come up paused; see
        # EditorSession.engine_is_running). The "frames running" wait below
        # doubles as the proof of the resume.
        if not session.engine_is_running():
            session.send("command editor.pause_toggle")
        session.wait_token(SHELL_FRAME_TOKEN, 120, "frames running")

        # Step 0: nothing is broken yet, so no problem was ever reported.
        assert PROBLEMS_UPDATED_TOKEN not in session.read_log(), (
            "problems reported before any edit"
        )

        # Step 1: break the extension's source. The error lands on the file's
        # new last line, which is what the panel row must point at.
        text = original_source
        if not text.endswith("\n"):
            text += "\n"
        broken_line_number = text.count("\n") + 1
        session.edit_source(EXTENSION_SOURCE, text + BROKEN_LINE + "\n")
        print(f"  [OK] injected a type error at {EXTENSION_SOURCE.name}:{broken_line_number}")

        begin = len(session.read_log())
        session.wait(
            re.escape(PROBLEMS_UPDATED_TOKEN)
            + rf'\s+module="?{re.escape(EXTENSION_NAME)}"?\s+count=[1-9]',
            300,
            "failed reload reported as problems",
            begin,
        )
        session.wait(
            re.escape(PROBLEM_ROW_TOKEN) + r'\s+[^\n]*file="[^"]*pill_dummy_color\.rs"',
            60,
            "problem carries its file",
            begin,
        )
        match = re.search(
            re.escape(PROBLEM_ROW_TOKEN)
            + r'\s+module="?([^"\s]+)"?\s+level="?(\w+)"?\s+'
            r'file="([^"]*)"\s+line=(\d+)\s+text="(.*)"\s*$',
            session.read_log()[begin:],
            re.MULTILINE,
        )
        assert match, "the parsed problem line did not carry module/level/file/line/message"
        module_seen, level_seen, file_seen, line_seen, message_seen = match.groups()
        assert int(line_seen) == broken_line_number, (
            f"the row points at line {line_seen}, the error is at {broken_line_number}"
        )
        assert level_seen == "error", f"the problem's level is {level_seen}"
        assert module_seen == EXTENSION_NAME, f"the problem's module is {module_seen}"
        assert "E0308" in message_seen or "mismatched types" in message_seen, (
            f"the problem's message is not the injected error: {message_seen}"
        )
        assert file_seen.endswith("pill_dummy_color.rs"), file_seen
        print(f"  [OK] problem {module_seen}:{file_seen}:{line_seen} {level_seen}")

        session.wait(
            re.escape(TOAST_SHOWN_TOKEN) + r'\s+[^\n]*text="[^"]*reload failed',
            60,
            "failure toast reached the shell",
            begin,
        )
        session.wait(
            re.escape(PROBLEMS_PANEL_TOKEN) + r"\s+[^\n]*count=[1-9]",
            60,
            "the Problems panel took the revision",
            begin,
        )
        # The failed generation must keep running: the editor is still alive
        # and still drawing frames.
        assert session.process.poll() is None, "the editor exited on a failed reload"

        # Step 2: the Problems tab, then the row's click. The click asks the
        # executable to open the file at the error's line; the recorder proves
        # what arrived.
        begin = len(session.read_log())
        session.send("tab Problems")
        session.wait(
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title="?Problems',
            30,
            "Problems tab selected",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate problems.open.0")
        session.wait(
            re.escape(PROBLEMS_ROW_ACTIVATED_TOKEN)
            + rf'\s+file="[^"]*pill_dummy_color\.rs"\s+line={broken_line_number}',
            60,
            "the row's click asked to open the file",
            begin,
        )
        session.wait(
            re.escape(OPEN_FILE_TOKEN)
            + rf'\s+file="[^"]*pill_dummy_color\.rs"\s+line={broken_line_number}',
            60,
            "the executable handed the file to the configured command",
            begin,
        )
        deadline = time.monotonic() + 30
        recorded = ""
        while time.monotonic() < deadline:
            if open_record.is_file():
                recorded = open_record.read_text(encoding="utf-8", errors="replace")
                if "pill_dummy_color.rs" in recorded:
                    break
            # Poll until the open recorder names the edited file.
            time.sleep(0.5)
        assert "pill_dummy_color.rs" in recorded, (
            f"the open recorder never saw the file: {recorded!r}"
        )
        recorded_match = re.search(r"([A-Za-z]:\\[^\s]*pill_dummy_color\.rs):(\d+)", recorded)
        assert recorded_match, f"the open recorder saw no file:line: {recorded!r}"
        recorded_file, recorded_line = recorded_match.groups()
        assert int(recorded_line) == broken_line_number, (
            f"the open recorder saw line {recorded_line}, the error is at {broken_line_number}"
        )
        # The link is only working if the path it carries is the real file.
        assert Path(recorded_file).is_file(), (
            f"the open request's path is not a file: {recorded_file}"
        )
        print(f"  [OK] open recorder saw {recorded.strip()}")

        # Step 3: fix the source; the reload succeeds and the panel clears.
        session.edit_source(EXTENSION_SOURCE, original_source)
        begin = len(session.read_log())
        session.wait_token(
            EXTENSION_RELOADED_TOKEN, 300, "the fixed extension reloaded", begin
        )
        session.wait(
            re.escape(PROBLEMS_UPDATED_TOKEN)
            + rf'\s+module="?{re.escape(EXTENSION_NAME)}"?\s+count=0',
            60,
            "the problem list for the module cleared",
            begin,
        )
        session.wait(
            re.escape(TOAST_SHOWN_TOKEN) + r'\s+[^\n]*text="[^"]*reloaded in \d+ ms',
            60,
            "the reload toast carries its build time",
            begin,
        )
        # And the panel's revision moved for the clearing too.
        session.wait(
            re.escape(PROBLEMS_PANEL_TOKEN) + r"\s+[^\n]*count=0",
            60,
            "the Problems panel saw the clear",
            begin,
        )

        # Step 4: a patched body that panics every frame is rolled back
        # automatically, reported, and the editor keeps running on the
        # previous code (D22/D33).
        begin = len(session.read_log())
        assert SHELL_BODY_ANCHOR in original_shell, "the shell body anchor moved"
        session.edit_source(
            SHELL_SOURCE,
            original_shell.replace(
                SHELL_BODY_ANCHOR, SHELL_PANIC_LINE + "\n" + SHELL_BODY_ANCHOR, 1
            ),
        )
        session.wait(
            re.escape(HOT_PREFIX_TOKEN) + r"\S*editor_shell_update\S* LIVE",
            300,
            "the panicking shell body patched in",
            begin,
        )
        # Kept inline on purpose: the line embeds the injected panic message,
        # so no stable token can pin it.
        session.wait(
            r'frame error: system "?editor_shell"? failed: panicked: reload feedback rollback probe',
            120,
            "the panic is contained and reported once",
            begin,
        )
        session.wait_token(ROLLBACK_TOKEN, 120, "the host rolled the panicking patch back", begin)
        rolled_back = session.wait(
            re.escape(TOAST_SHOWN_TOKEN)
            + r'\s+[^\n]*text="[^"]*rolled back to generation 0 after repeated panics',
            120,
            "the rollback toast reached the shell",
            begin,
        )
        session.wait(
            re.escape(PROBLEMS_UPDATED_TOKEN)
            + r'\s+module="[^"]*editor_shell_update"\s+count=1',
            60,
            "the rollback is a Problems row",
            begin,
        )
        # Frames keep arriving after that point: the editor survived on the
        # previous code.
        session.wait_token(
            SHELL_FRAME_TOKEN, 60, "frames continue after the rollback", rolled_back
        )

        # Step 5: restoring the body is a new edit, so a new patch applies (the
        # rollback suppressed only the rejected one).
        begin = len(session.read_log())
        session.edit_source(SHELL_SOURCE, original_shell)
        session.wait(
            re.escape(HOT_PREFIX_TOKEN) + r"\S*editor_shell_update\S* LIVE",
            300,
            "the restored shell body patched in",
            begin,
        )

        # Step 6: a clean exit.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"the editor exited {status}"
        print("  [OK] the editor exited cleanly")
        return 0
    finally:
        session.close()


if __name__ == "__main__":
    sys.exit(main())

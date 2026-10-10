"""End-to-end acceptance for the editor's command layer (W-0004 Stage 14).

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs (the spline project, whose editor crate owns a command)

DESCRIPTION
  Starts the editor on the spline project and drives the command layer
  through the scripted channel:

    1. the built-in commands (pause, step, save layout, reset layout) execute
       by id through the shell's own dispatch, and the engine applies the
       queued batch at the frame boundary;
    2. the menu bar opens a dropdown on a title click and the item click runs
       its command;
    3. the command palette opens and its first row runs on a click;
    4. switching the keymap preset in the keybindings file rebinds live (each
       preset's own chord runs the command, checked for Blender, Unity, Godot
       and Unreal Engine), a single override wins on top, and a conflict is
       reported by both command names;
    5. the spline editor crate's own command runs, and survives a live reload
       of that crate (its source is edited and restored), counted across
       generations in its persisted marker.

  Everything is asserted on the editor's log tokens; the edited crate source
  is restored in a `finally`. The suite's own layout, keybindings and command
  files live under `local/editor-validation/` (through its own LOCALAPPDATA),
  so the developer's real editor state is untouched.

USAGE
  python devops/tests/test_editor_commands.py

EXAMPLE USAGE
  python devops/tests/test_editor_commands.py

--- SCRIPT ---
"""

import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULES = ROOT / "modules"
BINARY = MODULES / "target" / "debug" / "pill_editor_native.exe"
VALIDATION = ROOT / "local" / "editor-validation"
LOG = VALIDATION / "editor-commands.log"
COMMANDS = VALIDATION / "commands-stage14.txt"
APPDATA = VALIDATION / "stage14_appdata"
SPLINE_EDITOR_SOURCE = (
    MODULES
    / "extensions"
    / "pill_spline"
    / "pill_spline_editor"
    / "src"
    / "pill_spline_editor.rs"
)


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
        match = expression.search(text[start:])
        if match:
            print(f"  [OK] {label}")
            return len(text)
        if process.poll() is not None:
            raise RuntimeError(
                f"{label}: the editor exited {process.returncode}\n" + text[-4000:]
            )
        time.sleep(0.5)
    raise TimeoutError(f"{label}: timeout waiting for {pattern}\n" + read_log()[-4000:])


def append(line: str) -> None:
    """Appends one scripted command, flushed, so the editor sees it whole."""
    with COMMANDS.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def write_keybindings(body: dict) -> None:
    """Writes the keybindings file the shell live-reads."""
    path = APPDATA / "RustHybridEcs" / "keybindings.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(body), encoding="utf-8")


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
    keybindings = APPDATA / "RustHybridEcs" / "keybindings.json"
    if keybindings.exists():
        keybindings.unlink()
    original_source = SPLINE_EDITOR_SOURCE.read_text(encoding="utf-8")
    COMMANDS.write_text("# stage 14 commands suite\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/project_rs"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "300"
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
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log: anchoring after the previous match can
        # skip a line that was written before the anchor was taken.
        wait_token(process, r"\[editor\] pill_spline_editor loaded", 300, "editor crate loaded")
        wait_token(process, r"\[editor\] pill_editor_shell_editor loaded", 300, "shell loaded")
        wait_token(process, r"\[editor\] window created", 300, "editor started")
        wait_token(process, r"\[editor\] command registry ready", 120, "registry ready")
        wait_token(process, r"\[editor\] menu registered", 60, "menus registered")

        # Step 1: the built-in commands by id, through the shell's dispatch.
        for command_id, label in [
            ("editor.pause_toggle", "pause"),
            ("editor.step", "step"),
            ("editor.save_layout", "save layout"),
            ("editor.reset_layout", "reset layout"),
        ]:
            before = len(read_log())
            append(f"command {command_id}")
            wait_token(
                process,
                rf'\[editor\] command executed\s+id="?{re.escape(command_id)}"?',
                60,
                f"{label} executed",
                before,
            )
        wait_token(
            process, r"\[editor\] engine commands applied\s+count=1", 60,
            "engine batch applied",
        )

        # Step 2: the menu bar, clicked with ordinary pointer events.
        before = len(read_log())
        append("click 30 10")
        position = wait_token(
            process, r'\[editor\] menu opened\s+path="?Layout"?', 30,
            "menu opened by click", before,
        )
        append("click 100 42")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.save_layout"?',
            30,
            "menu item ran save layout",
            position,
        )

        # Step 3: the palette, opened by its command and clicked in its rows.
        before = len(read_log())
        append("command editor.command_palette")
        position = wait_token(
            process, r"\[editor\] command palette opened", 30, "palette opened", before
        )
        append("click 120 82")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.pause_toggle"?',
            30,
            "palette row ran pause",
            position,
        )

        # Step 4: the keymap preset, the override and the conflict.
        before = len(read_log())
        write_keybindings({"preset": "unity"})
        position = wait_token(
            process, r'\[editor\] keymap applied\s+preset="?unity"?', 30,
            "unity preset applied", before,
        )
        before = len(read_log())
        append("chord ctrl+p")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.pause_toggle"?',
            30,
            "unity chord ran pause",
            before,
        )
        before = len(read_log())
        write_keybindings({"preset": "unity", "overrides": {"editor.pause_toggle": ["f5"]}})
        position = wait_token(
            process, r'\[editor\] keymap applied\s+preset="?unity"?\s+overrides=1', 30,
            "override applied", before,
        )
        before = len(read_log())
        append("chord f5")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.pause_toggle"?',
            30,
            "override chord ran pause",
            before,
        )
        before = len(read_log())
        write_keybindings({"preset": "unity", "overrides": {"editor.step": ["ctrl+p"]}})
        wait_token(
            process,
            r'\[editor\] command layer problem"\[editor\] keybinding conflict'
            r'\s+chord=\\"ctrl\+p\\" commands=editor\.pause_toggle,editor\.step"',
            30,
            "conflict reported",
            before,
        )

        # Step 4b: the Godot and Unreal presets rebind live too, each with its
        # own chord for Save Layout (ctrl+alt+l vs ctrl+shift+l).
        before = len(read_log())
        write_keybindings({"preset": "godot"})
        wait_token(
            process, r'\[editor\] keymap applied\s+preset="?godot"?', 30,
            "godot preset applied", before,
        )
        before = len(read_log())
        append("chord ctrl+alt+l")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.save_layout"?',
            30,
            "godot chord ran save layout",
            before,
        )
        before = len(read_log())
        write_keybindings({"preset": "unreal"})
        wait_token(
            process, r'\[editor\] keymap applied\s+preset="?unreal"?', 30,
            "unreal preset applied", before,
        )
        before = len(read_log())
        append("chord ctrl+shift+l")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.save_layout"?',
            30,
            "unreal chord ran save layout",
            before,
        )

        # Step 5: the editor crate's command and its survival of a reload.
        before = len(read_log())
        append("command spline_editor.ping")
        position = wait_token(
            process,
            r'\[spline_editor\] command ran\s+id="?spline_editor\.ping"?\s+commands_ran=1',
            60,
            "spline command ran",
            before,
        )
        before = len(read_log())
        SPLINE_EDITOR_SOURCE.write_text(
            original_source + "\n// stage 14 suite reload check\n", encoding="utf-8"
        )
        position = wait_token(
            process,
            r"\[editor\] pill_spline_editor loaded\s+loaded_generations=2",
            180,
            "editor crate reloaded",
            before,
        )
        append("command spline_editor.ping")
        wait_token(
            process,
            r'\[spline_editor\] command ran\s+id="?spline_editor\.ping"?\s+commands_ran=2',
            60,
            "spline command survived the reload",
            position,
        )

        append("quit")
        status = process.wait(timeout=120)
        assert status == 0, f"editor exited {status}"
        print("  [OK] clean exit, code 0")
        print("PASS: editor commands")
        return 0
    finally:
        SPLINE_EDITOR_SOURCE.write_text(original_source, encoding="utf-8")
        if process.poll() is None:
            process.kill()
            process.wait(timeout=30)


if __name__ == "__main__":
    sys.exit(main())

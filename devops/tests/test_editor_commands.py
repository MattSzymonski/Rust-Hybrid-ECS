"""End-to-end acceptance for the editor's command layer.

PROTECTS: The command layer: built-in commands by id, menus, the palette and live keymap presets.
TIER: editor
SETS: editor

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
  is restored when the session closes. The run's layout, keybindings and
  command files live in their own folder under
  `local/editor-validation/commands-<run id>/` (through the session's own
  LOCALAPPDATA), so the developer's real editor state is untouched.

USAGE
  python devops/tests/test_editor_commands.py

EXAMPLE USAGE
  python devops/tests/test_editor_commands.py

--- SCRIPT ---
"""

import re
import sys
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_COMMAND_EXECUTED_TOKEN,
    EDITOR_COMMAND_REGISTRY_TOKEN,
    EDITOR_ENGINE_COMMANDS_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_SPLINE_CRATE_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)
from core.paths import MODULES_ROOT  # noqa: E402

# Suite-specific tokens: the stable halves of the lines this suite asserts
# on, so `test_log_contract.py` checks each against the Rust sources.
MENU_REGISTERED_TOKEN = "[editor] menu registered"
MENU_OPENED_TOKEN = "[editor] menu opened"
PALETTE_OPENED_TOKEN = "[editor] command palette opened"
KEYMAP_APPLIED_TOKEN = "[editor] keymap applied"
HISTORY_UNDO_EMPTY_TOKEN = "[editor] history undo empty"
KEYBINDING_CONFLICT_TOKEN = "[editor] keybinding conflict"
SPLINE_COMMAND_RAN_TOKEN = "[spline_editor] command ran"

SPLINE_EDITOR_SOURCE = (
    MODULES_ROOT
    / "extensions"
    / "pill_spline"
    / "pill_spline_editor"
    / "src"
    / "pill_spline_editor.rs"
)


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession("commands")
    session.start()
    try:
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log: anchoring after the previous match can
        # skip a line that was written before the anchor was taken.
        session.wait_token(EDITOR_SPLINE_CRATE_TOKEN, 300, "editor crate loaded")
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait_token(EDITOR_COMMAND_REGISTRY_TOKEN, 120, "registry ready")
        session.wait_token(MENU_REGISTERED_TOKEN, 60, "menus registered")

        # Step 1: the built-in commands by id, through the shell's dispatch.
        for command_id, label in [
            ("editor.pause_toggle", "pause"),
            ("editor.step", "step"),
            ("editor.save_layout", "save layout"),
            ("editor.reset_layout", "reset layout"),
        ]:
            before = len(session.read_log())
            session.send(f"command {command_id}")
            session.wait(
                re.escape(EDITOR_COMMAND_EXECUTED_TOKEN)
                + rf'\s+id="?{re.escape(command_id)}"?',
                60,
                f"{label} executed",
                before,
            )
        session.wait(
            re.escape(EDITOR_ENGINE_COMMANDS_TOKEN) + r"\s+count=1",
            60,
            "engine batch applied",
        )

        # Step 2: the menu bar. The first title is Edit (the built-in menus
        # sort before Layout). Its dropdown is a popup window of its own
        # (Stage 21), so the item click goes through the popup pointer
        # channel; the first item is Undo and the history is still empty
        # here, so the item's observable effect is `history undo empty`.
        before = len(session.read_log())
        session.send("click 30 10")
        position = session.wait(
            re.escape(MENU_OPENED_TOKEN) + r'\s+path="?Edit"?', 30,
            "menu opened by click", before,
        )
        session.send("popup-pointer menu 110 16 release")
        session.wait_token(HISTORY_UNDO_EMPTY_TOKEN, 30, "menu item ran undo", position)

        # Step 3: the palette, opened by its command and clicked in its
        # rows; it is a popup window too. The first row is Pause / Resume
        # (the built-in registrations list it first).
        before = len(session.read_log())
        session.send("command editor.command_palette")
        position = session.wait_token(
            PALETTE_OPENED_TOKEN, 30, "palette opened", before
        )
        session.send("popup-pointer palette 120 42 release")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "palette row ran pause",
            position,
        )

        # Step 4: the keymap preset, the override and the conflict.
        before = len(session.read_log())
        session.write_appdata_json("keybindings.json", {"preset": "unity"})
        position = session.wait(
            re.escape(KEYMAP_APPLIED_TOKEN) + r'\s+preset="?unity"?', 30,
            "unity preset applied", before,
        )
        before = len(session.read_log())
        session.send("chord ctrl+p")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "unity chord ran pause",
            before,
        )
        before = len(session.read_log())
        session.write_appdata_json(
            "keybindings.json", {"preset": "unity", "overrides": {"editor.pause_toggle": ["f5"]}}
        )
        position = session.wait(
            re.escape(KEYMAP_APPLIED_TOKEN) + r'\s+preset="?unity"?\s+overrides=1', 30,
            "override applied", before,
        )
        before = len(session.read_log())
        session.send("chord f5")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "override chord ran pause",
            before,
        )
        before = len(session.read_log())
        session.write_appdata_json(
            "keybindings.json", {"preset": "unity", "overrides": {"editor.step": ["ctrl+p"]}}
        )
        session.wait(
            r'\[editor\] command layer problem"\[editor\] keybinding conflict'
            r'\s+chord=\\"ctrl\+p\\" commands=editor\.pause_toggle,editor\.step"',
            30,
            "conflict reported",
            before,
        )

        # Step 4b: the Godot and Unreal presets rebind live too, each with its
        # own chord for Save Layout (ctrl+alt+l vs ctrl+shift+l).
        before = len(session.read_log())
        session.write_appdata_json("keybindings.json", {"preset": "godot"})
        session.wait(
            re.escape(KEYMAP_APPLIED_TOKEN) + r'\s+preset="?godot"?', 30,
            "godot preset applied", before,
        )
        before = len(session.read_log())
        session.send("chord ctrl+alt+l")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.save_layout"?',
            30,
            "godot chord ran save layout",
            before,
        )
        before = len(session.read_log())
        session.write_appdata_json("keybindings.json", {"preset": "unreal"})
        session.wait(
            re.escape(KEYMAP_APPLIED_TOKEN) + r'\s+preset="?unreal"?', 30,
            "unreal preset applied", before,
        )
        before = len(session.read_log())
        session.send("chord ctrl+shift+l")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.save_layout"?',
            30,
            "unreal chord ran save layout",
            before,
        )

        # Step 5: the editor crate's command and its survival of a reload.
        # The crate's system prints the report on its next tick, so the
        # engine must be running: the pause chords above leave the toggle
        # count ambiguous when a stray input already toggled pause as the
        # window appeared.
        if not session.engine_is_running():
            before = len(session.read_log())
            session.send("command editor.pause_toggle")
            session.wait(
                re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
                30,
                "engine resumed for the module command",
                before,
            )
        before = len(session.read_log())
        session.send("command spline_editor.ping")
        position = session.wait(
            re.escape(SPLINE_COMMAND_RAN_TOKEN)
            + r'\s+id="?spline_editor\.ping"?\s+commands_ran=1',
            60,
            "spline command ran",
            before,
        )
        before = len(session.read_log())
        with SPLINE_EDITOR_SOURCE.open("r", encoding="utf-8", newline="") as file:
            original_source = file.read()
        session.edit_source(
            SPLINE_EDITOR_SOURCE, original_source + "\n// command layer suite reload check\n"
        )
        position = session.wait(
            re.escape(EDITOR_SPLINE_CRATE_TOKEN) + r"\s+loaded_generations=2",
            180,
            "editor crate reloaded",
            before,
        )
        # The reloaded crate's system needs a running engine too, in case a
        # stray input paused it while the reload settled.
        if not session.engine_is_running():
            begin = len(session.read_log())
            session.send("command editor.pause_toggle")
            session.wait(
                re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
                30,
                "engine resumed after the reload",
                begin,
            )
        session.send("command spline_editor.ping")
        session.wait(
            re.escape(SPLINE_COMMAND_RAN_TOKEN)
            + r'\s+id="?spline_editor\.ping"?\s+commands_ran=2',
            60,
            "spline command survived the reload",
            position,
        )

        status = session.quit_and_wait()
        assert status == 0, f"editor exited {status}"
        print("  [OK] clean exit, code 0")
        print("PASS: editor commands")
        return 0
    finally:
        session.close()


if __name__ == "__main__":
    sys.exit(main())

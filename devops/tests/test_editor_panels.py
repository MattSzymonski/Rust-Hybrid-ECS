"""End-to-end acceptance for the panel registration API.

PROTECTS: The panel registration API: an extension panel opens, records undo steps and live-patches.
TIER: editor
SETS: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor.exe
  - examples/project_rs with its spline editor crate (the suite edits its
    panel body and restores it)

DESCRIPTION
  Starts the editor on the spline project and drives the panel layer:

    1. `pill_spline_editor`'s Spline Tool registers through the panel API
       (the registry line lists it) and opening it from the Window menu
       docks its tab and builds the panel;
    2. a control-point edit through the panel's command is an undo step;
       undo restores the previous count;
    3. Re-seed runs through the module's own system and logs its points;
    4. a body-only edit of the panel function live-patches (`LIVE ms`);
    5. editing the runtime `pill_spline` crate reloads it and the editor
       crate, and the panel builds again in the new generation (kept open);
    5b. the free-standing windows: the Spline Tool opens a tool
       window and a confirm modal, the modal blocks the editor's input and
       answers its button as a command, a reload closes the modal and
       reopens the tool window, a disappeared panel closes its window, a
       user close sticks, and the over-budget fixture's warning reaches the
       toast stack;
    6. the harness' panic panel is contained (a warn line, the shell keeps
       painting) and an unregistered panel shows the placeholder;
    7. the editor exits cleanly.

  Everything is asserted on the editor's log tokens; both edited sources are
  restored when the session closes. The suite runs under its own LOCALAPPDATA
  sandbox.

USAGE
  python devops/tests/test_editor_panels.py

  The run's log and command file land in their own folder under
  `local/editor-validation/panels-<run id>/`.

EXAMPLE USAGE
  python devops/tests/test_editor_panels.py

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
    EDITOR_COMMAND_EXECUTED_TOKEN,
    EDITOR_COMMAND_REGISTRY_TOKEN,
    EDITOR_HISTORY_PUSH_TOKEN,
    EDITOR_HISTORY_UNDO_TOKEN,
    EDITOR_PANEL_REGISTRY_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)
from core.paths import MODULES_ROOT  # noqa: E402

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
SPLINE_EDITOR_LOADED_TOKEN = "[editor] pill_spline_editor loaded"
PANEL_REGISTERED_TOKEN = "[editor] panel registered"
PANEL_FOCUSED_TOKEN = "[editor] panel focused"
MODULE_PANEL_BUILT_TOKEN = "[editor] module panel built"
MODULE_PANEL_PANICKED_TOKEN = "[editor] module panel panicked"
MODULE_PANEL_MISSING_TOKEN = "[editor] module panel missing"
PANEL_OVER_BUDGET_TOKEN = "[editor] panel over budget"
PANEL_BUDGET_TOAST_TOKEN = "[editor] panel budget toast"
ENTITY_LIST_TOKEN = "[test] entity"
FIELD_VALUE_TOKEN = "[test] field"
CONTROL_POINT_ADDED_TOKEN = "[spline_editor] control point added"
RESEEDED_TOKEN = "[spline_editor] reseeded"
SPLINE_COMMAND_RAN_TOKEN = "[spline_editor] command ran"
MODULE_DLL_LOADED_TOKEN = "module DLL loaded successfully"
WIDGET_ACTIVATED_TOKEN = "[editor] widget activated"
WINDOW_WIDGET_ACTIVATED_TOKEN = "[editor] window widget activated"
WINDOW_OPENED_TOKEN = "[editor] window opened"
WINDOW_PANEL_BUILT_TOKEN = "[editor] window panel built"
WINDOW_CLOSED_TOKEN = "[editor] window closed"
WINDOW_REOPENED_TOKEN = "[editor] window reopened"
MODAL_BLOCKS_TOKEN = "[editor] modal blocks editor input"
MODAL_ANSWERED_TOKEN = "[editor] modal answered"
MODAL_CLOSED_TOKEN = "[editor] modal closed"
CONTEXT_MENU_OPENED_TOKEN = "[editor] context menu opened"
LIVE_TOKEN = "LIVE "

SPLINE_EDITOR_SOURCE = (
    MODULES_ROOT
    / "extensions"
    / "pill_spline"
    / "pill_spline_editor"
    / "src"
    / "pill_spline_editor.rs"
)
SPLINE_SOURCE = (
    MODULES_ROOT / "extensions" / "pill_spline" / "pill_spline" / "src" / "pill_spline.rs"
)


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession("panels", test_seconds=600)
    with SPLINE_EDITOR_SOURCE.open("r", encoding="utf-8", newline="") as file:
        original_editor = file.read()
    with SPLINE_SOURCE.open("r", encoding="utf-8", newline="") as file:
        original_runtime = file.read()
    session.start()
    try:
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log: anchoring after the previous match can
        # skip a line that was written before the anchor was taken.
        session.wait_token(SPLINE_EDITOR_LOADED_TOKEN, 300, "editor crate loaded")
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait_token(EDITOR_COMMAND_REGISTRY_TOKEN, 120, "registry ready")
        # The module-system waits below need the project ticking (the editor
        # can come up paused - see EditorSession.engine_is_running - and a
        # paused, idle editor sleeps at its heartbeat rate).
        if not session.engine_is_running():
            resume_begin = len(session.read_log())
            session.send("command editor.pause_toggle")
            session.wait(
                re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
                30,
                "engine resumed",
                resume_begin,
            )

        # Step 1: the registry lists the crate's panel and the harness'
        # panic fixture. These are startup lines, so they are searched from
        # the top of the log.
        session.wait(
            re.escape(PANEL_REGISTERED_TOKEN)
            + r'\s+id="?spline_tool"?\s+title="?Spline Tool"?\s+owner="?pill_spline_editor"?',
            60,
            "spline tool registered",
        )
        session.wait(
            re.escape(EDITOR_PANEL_REGISTRY_TOKEN) + r"\s+panels=4",
            60,
            "panel registry ready",
        )

        # Step 2: open the panel from the Window menu command and watch it
        # build in the new tab.
        before = len(session.read_log())
        session.send("command editor.panel.spline_tool")
        session.wait(
            re.escape(PANEL_FOCUSED_TOKEN) + r'\s+id="?spline_tool"?\s+existing=false',
            30,
            "panel focused (new tab)",
            before,
        )
        session.wait(
            re.escape(MODULE_PANEL_BUILT_TOKEN) + r'\s+id="?spline_tool"?',
            30,
            "panel built",
            before,
        )

        # Step 3: a control-point edit through the panel's command is one
        # recorded undo step. The demo project's own system rewrites the
        # spline every tick, so the *value* is the sim's; the step itself is
        # what is asserted here (and the stable `elo` field below carries
        # the value-level check).
        before = len(session.read_log())
        session.send("dump entities")
        session.wait(re.escape(ENTITY_LIST_TOKEN) + r"\s", 30, "entities listed", before)
        entity = session.find(
            re.escape(ENTITY_LIST_TOKEN) + r'\s+entity=(\S+)\s+components="?[^"\s]*Spline(,|")',
            before,
        ).group(1)
        print(f"  [OK] spline entity {entity}")

        before = len(session.read_log())
        session.send("command spline_editor.add_point")
        session.wait(
            re.escape(CONTROL_POINT_ADDED_TOKEN)
            + r"\s+entity=\S+\s+control_points=\d+",
            30,
            "add point ran the module system",
            before,
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="spline_editor\.add_point"',
            30,
            "add point recorded",
            before,
        )
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="spline_editor\.add_point"',
            30,
            "add point undone",
            before,
        )

        # A stable field proves undo/redo values end to end.
        before = len(session.read_log())
        session.send(f"dump field {entity} Spline elo")
        session.wait(re.escape(FIELD_VALUE_TOKEN) + r"\s", 30, "original elo read", before)
        original_elo = session.find(r"value=F32\(([^)]+)\)", before).group(1)
        before = len(session.read_log())
        session.send(f"edit set {entity} Spline elo 3.5f")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="set Spline\.elo"',
            30,
            "elo edit recorded",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} Spline elo")
        session.wait(r"value=F32\(3\.5\)", 30, "elo edit applied", before)
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="set Spline\.elo"',
            30,
            "elo edit undone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} Spline elo")
        session.wait(
            r"value=F32\(" + re.escape(original_elo) + r"\)",
            30,
            "elo restored",
            before,
        )

        # Step 4: re-seed through the module's own system.
        before = len(session.read_log())
        session.send("command spline_editor.reseed")
        session.wait(
            re.escape(RESEEDED_TOKEN) + r"\s+splines=1\s+control_points=16",
            30,
            "re-seed ran",
            before,
        )

        # Step 5: a body-only edit of the panel function live-patches.
        before = len(session.read_log())
        patched = original_editor.replace(
            'builder.heading("Spline Tool")', 'builder.heading("Spline Tool v2")', 1
        )
        assert patched != original_editor, "panel heading marker not found"
        session.edit_source(SPLINE_EDITOR_SOURCE, patched)
        session.wait(re.escape(LIVE_TOKEN) + r"\d+ ms", 120, "panel body live-patched", before)
        session.edit_source(SPLINE_EDITOR_SOURCE, original_editor)

        # Step 6: editing the runtime crate reloads it and the editor crate;
        # the panel is kept open - its tab still exists (focus reports
        # `existing=true`) and the re-registered module command still runs.
        before = len(session.read_log())
        session.edit_source(
            SPLINE_SOURCE, original_runtime + "\n// panels suite reload check\n"
        )
        session.wait(
            re.escape(MODULE_DLL_LOADED_TOKEN)
            + r'\s+module="?pill_spline"?\s+abi_version=\d+',
            180,
            "runtime crate reloaded",
            before,
        )
        session.wait(
            re.escape(SPLINE_EDITOR_LOADED_TOKEN) + r"\s+loaded_generations=2",
            180,
            "editor crate reloaded",
            before,
        )
        before_wait = len(session.read_log())
        session.send("command editor.panel.spline_tool")
        session.wait(
            re.escape(PANEL_FOCUSED_TOKEN) + r'\s+id="?spline_tool"?\s+existing=true',
            30,
            "panel tab kept across the reload",
            before_wait,
        )
        before_wait = len(session.read_log())
        session.send("command spline_editor.add_point")
        session.wait(
            re.escape(CONTROL_POINT_ADDED_TOKEN)
            + r"\s+entity=\S+\s+control_points=\d+",
            30,
            "module command re-registered",
            before_wait,
        )
        session.wait(
            re.escape(SPLINE_COMMAND_RAN_TOKEN)
            + r"\s+id=spline_editor\.add_point\s+commands_ran=3",
            30,
            "command ran in the new generation",
            before_wait,
        )
        # Restoring the runtime source in-step would trigger another reload;
        # the sources stay edited until the `finally` instead, because rapid
        # consecutive reload cycles hit a pre-existing crash (W-0009).

        # Step 6b: the free-standing windows. The Spline Tool
        # detaches into a tool window of its own, opens a confirm modal, the
        # modal blocks the editor's own input and answers its button as a
        # command; a later reload closes the modal and reopens the tool
        # window; dropping the panel's registration closes its window; a
        # user close sticks; and the over-budget fixture reaches the toast.
        before = len(session.read_log())
        session.send("activate spline.open_window")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + r'\s+name="?spline\.open_window"?',
            30,
            "detach button pressed",
            before,
        )
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?spline_tool_window"?\s+kind="?tool"?',
            30,
            "tool window opened",
            before,
        )
        session.wait(
            re.escape(WINDOW_PANEL_BUILT_TOKEN)
            + r'\s+id="?spline_tool_window"?\s+panel="?spline_tool"?',
            30,
            "tool window panel built",
            before,
        )
        session.wait(
            re.escape(EDITOR_WINDOW_CREATED_TOKEN)
            + r'\s+id="?spline_tool_window"?\s+kind="?tool"?',
            30,
            "tool window's platform window created",
            before,
        )
        before = len(session.read_log())
        session.send("activate spline.confirm_reseed")
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?spline_reseed_confirm"?\s+kind="?modal"?',
            30,
            "confirm modal opened",
            before,
        )
        session.wait(
            re.escape(WINDOW_PANEL_BUILT_TOKEN)
            + r'\s+id="?spline_reseed_confirm"?\s+panel="?spline_reseed"?',
            30,
            "modal body panel built",
            before,
        )

        # The modal blocks the editor's own input: a right-click that would
        # open the context menu is dropped, and the drop is stated once.
        before = len(session.read_log())
        session.send("rightclick 300 300")
        session.wait(
            re.escape(MODAL_BLOCKS_TOKEN) + r'\s+id="?spline_reseed_confirm"?',
            30,
            "modal blocked the editor's input",
            before,
        )
        assert CONTEXT_MENU_OPENED_TOKEN not in session.read_log()[before:], (
            "a blocked right-click still opened the context menu"
        )

        # The modal answers its button as a command: Enter-equivalent
        # activation of the first button runs spline_editor.reseed.
        before = len(session.read_log())
        session.send("activate window.spline_reseed_confirm.button.0")
        session.wait(
            re.escape(WINDOW_WIDGET_ACTIVATED_TOKEN)
            + r'\s+name="?window\.spline_reseed_confirm\.button\.0"?',
            30,
            "modal button activated",
            before,
        )
        session.wait(
            re.escape(MODAL_ANSWERED_TOKEN)
            + r'\s+id="?spline_reseed_confirm"?\s+button="?Re-seed"?\s+command="?spline_editor\.reseed"?',
            30,
            "modal answered with its command",
            before,
        )
        session.wait(
            re.escape(RESEEDED_TOKEN) + r"\s+splines=\d+\s+control_points=16",
            30,
            "the modal's command ran the module system",
            before,
        )

        # A reload of the module closes the modal - its question belonged to
        # the old generation - and reopens the tool window on the new one.
        # The editor crate's own source is edited here (a comment-only line),
        # so the reload is its direct one; the runtime source stays as step 6
        # left it.
        before = len(session.read_log())
        session.send("activate spline.confirm_reseed")
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?spline_reseed_confirm"?',
            30,
            "modal opened again",
            before,
        )
        before = len(session.read_log())
        session.edit_source(
            SPLINE_EDITOR_SOURCE, original_editor + "\n// panels suite window reload check\n"
        )
        session.wait(
            re.escape(SPLINE_EDITOR_LOADED_TOKEN) + r"\s+loaded_generations=3",
            180,
            "editor crate reloaded for the window check",
            before,
        )
        session.wait(
            re.escape(MODAL_CLOSED_TOKEN) + r'\s+id="?spline_reseed_confirm"?\s+reason="?reload"?',
            60,
            "reload closed the modal",
            before,
        )
        session.wait(
            re.escape(WINDOW_REOPENED_TOKEN) + r'\s+id="?spline_tool_window"?',
            60,
            "reload reopened the tool window",
            before,
        )

        # The panel-budget warning reaches the toast stack:
        # the fixture panel builds past the budget on
        # every build, and the open tool window keeps the editor taking
        # frames, so 120 consecutive builds trip the badge and the toast.
        before = len(session.read_log())
        session.send("panel budget_test")
        session.wait(
            re.escape(PANEL_FOCUSED_TOKEN) + r'\s+id="?budget_test"?',
            30,
            "budget fixture focused",
            before,
        )
        session.wait(
            re.escape(PANEL_OVER_BUDGET_TOKEN) + r'\s+id="?budget_test"?',
            120,
            "budget fixture tripped its budget",
            before,
        )
        session.wait(
            re.escape(PANEL_BUDGET_TOAST_TOKEN) + r'\s+id="?budget_test"?',
            30,
            "budget warning raised a toast",
            before,
        )

        # A tool window the user closes stays closed: no later frame reopens
        # it - a reopen only ever happens when a reload replaces the code
        # under a window that is still open.
        before_close = len(session.read_log())
        session.send("window-dismiss spline_tool_window")
        session.wait(
            re.escape(WINDOW_CLOSED_TOKEN)
            + r'\s+id="?spline_tool_window"?\s+kind="?tool"?\s+reason="?platform"?',
            30,
            "user-closed tool window",
            before_close,
        )
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "post-close frame check",
            before_close,
        )
        after_check = len(session.read_log())
        assert WINDOW_REOPENED_TOKEN not in session.read_log()[before_close:after_check], (
            "a user-closed window reopened on its own"
        )
        session.send("command editor.pause_toggle")

        # With the panels gone (both registrations dropped, the module
        # reloaded), its tool window and its modal both close instead of
        # drawing through the dead registrations.
        before = len(session.read_log())
        session.send("panel spline_tool")
        session.wait(
            re.escape(PANEL_FOCUSED_TOKEN) + r'\s+id="?spline_tool"?',
            30,
            "spline tool focused for the unload check",
            before,
        )
        session.send("activate spline.open_window")
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?spline_tool_window"?',
            30,
            "tool window opened for the unload check",
            before,
        )
        session.send("activate spline.confirm_reseed")
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?spline_reseed_confirm"?',
            30,
            "modal opened for the unload check",
            before,
        )
        before = len(session.read_log())
        current = SPLINE_EDITOR_SOURCE.read_text(encoding="utf-8")
        gone = current.replace('"spline_tool",', '"spline_tool_gone",', 1).replace(
            '"spline_reseed",', '"spline_reseed_gone",', 1
        )
        assert gone != current and gone.count("_gone") == 2, (
            "spline_tool/spline_reseed registration markers not found"
        )
        session.edit_source(SPLINE_EDITOR_SOURCE, gone)
        session.wait(
            re.escape(WINDOW_CLOSED_TOKEN)
            + r'\s+id="?spline_tool_window"?\s+kind="?tool"?\s+reason="?panel-gone"?',
            180,
            "tool window closed with its panel gone",
            before,
        )
        session.wait(
            re.escape(WINDOW_CLOSED_TOKEN)
            + r'\s+id="?spline_reseed_confirm"?\s+kind="?modal"?\s+reason="?panel-gone"?',
            60,
            "modal closed with its panel gone",
            before,
        )

        # Step 7: the panic fixture is contained, and an unregistered panel
        # shows the placeholder.
        before = len(session.read_log())
        session.send("command editor.panel.containment_test")
        session.wait(
            re.escape(MODULE_PANEL_PANICKED_TOKEN) + r'.*id="?containment_test"?',
            60,
            "panicking panel contained",
            before,
        )
        # The raw panel request, not the Window-menu command: by now the
        # unload check above has left spline_tool unregistered, and the
        # command only exists while a panel is (a restored layout focus for
        # a panel whose module is gone takes this same path).
        before_wait = len(session.read_log())
        session.send("panel spline_tool")
        session.wait(
            re.escape(PANEL_FOCUSED_TOKEN) + r'\s+id="?spline_tool"?',
            30,
            "shell kept painting after the panic",
            before_wait,
        )
        before_wait = len(session.read_log())
        session.send("panel ghost_tool")
        session.wait(
            re.escape(PANEL_FOCUSED_TOKEN) + r'\s+id="?ghost_tool"?',
            30,
            "unknown panel focused",
            before_wait,
        )
        session.wait(
            re.escape(MODULE_PANEL_MISSING_TOKEN) + r'\s+id="?ghost_tool"?',
            30,
            "placeholder shown for the missing panel",
            before_wait,
        )

        # Step 8: exit cleanly.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status}"
        print("  [OK] clean exit, code 0")
    finally:
        session.close()
    print("PASS: editor panels")
    return 0


if __name__ == "__main__":
    sys.exit(main())

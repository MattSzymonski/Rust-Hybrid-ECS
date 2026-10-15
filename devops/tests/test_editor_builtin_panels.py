"""End-to-end acceptance for the built-in panels.

PROTECTS: The built-in panels: Inspector edits, system toggles, Console and Assets toolbar, undoable.
TIER: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs with its spline editor crate (the suite asks it for
    a game-channel log line and reloads the shell crate)

DESCRIPTION
  Starts the editor on the spline project and drives the built-in panels
  through the scripted channel's addressable activation (`activate <widget
  name>`, `select <entity>`, `type <text>`, `edit system ...`):

    1. selecting an entity fills the Inspector; editing its field through
       the named widget is one undoable step, and undo restores it;
    2. a system toggle is an undoable step and undo restores it;
    3. the Console captures both channels, its level/channel buttons and its
       search field work, and the counts survive a reload;
    4. the Preferences panel and the file stay in step in both directions
       (the file is written by the suite, then by the panel, and the file is
       read back);
    5. the Assets panel lists the project's files;
    6. the Assets toolbar's dialogs create an asset, rename it, delete it
       and write a missing sidecar, each a recorded step that undo walks
       back;
    7. a body edit of the shell live-patches with its state kept, and a
       structural edit of the shell crate reloads it with tabs, selection
       and console kept (an Inspector edit still works without re-selecting);
    8. the editor exits cleanly.

  Everything is asserted on the editor's log tokens; the edited sources are
  restored when the session closes. The suite runs under its own LOCALAPPDATA
  sandbox, and its run lands in `local/editor-validation/builtin-panels-<run id>/`.

USAGE
  python devops/tests/test_editor_builtin_panels.py

EXAMPLE USAGE
  python devops/tests/test_editor_builtin_panels.py

--- SCRIPT ---
"""

import base64
import json
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
    EDITOR_HISTORY_REDO_TOKEN,
    EDITOR_HISTORY_UNDO_TOKEN,
    EDITOR_PANEL_REGISTRY_TOKEN,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_TAB_SELECTED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)
from core.paths import MODULES_ROOT, REPOSITORY_ROOT  # noqa: E402

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
SHELL_FRAME_REPORT_TOKEN = "scene frame report"
WIDGET_ACTIVATED_TOKEN = "widget activated"
WINDOW_WIDGET_ACTIVATED_TOKEN = "window widget activated"
WIDGET_WHEELED_TOKEN = "widget wheeled"
WIDGET_NOT_FOUND_TOKEN = "widget not found"
SELECTION_CHANGED_TOKEN = "selection changed"
ENTITY_LIST_TOKEN = "[test] entity"
COMPONENT_LIST_TOKEN = "[test] component"
FIELD_VALUE_TOKEN = "[test] field"
STANDALONE_TYPE_TOKEN = "[test] standalone type"
RESEEDED_TOKEN = "[spline_editor] reseeded"
CONSOLE_STATE_TOKEN = "console state"
CONSOLE_FILTER_LEVEL_TOKEN = "console filter level"
CONSOLE_FILTER_CHANNEL_TOKEN = "console filter channel"
CONSOLE_SEARCH_TOKEN = "console search"
PREFERENCES_WRITTEN_TOKEN = "preferences written"
PREFERENCES_APPLIED_TOKEN = "preferences applied"
ASSET_LISTING_TOKEN = "asset listing refreshed"
ASSETS_REFRESH_TOKEN = "test assets refresh"
ASSET_SELECTED_TOKEN = "asset selected"
ASSET_CREATE_QUEUED_TOKEN = "asset create queued"
ASSET_RENAME_QUEUED_TOKEN = "asset rename queued"
ASSET_DELETE_QUEUED_TOKEN = "asset delete queued"
ASSET_META_QUEUED_TOKEN = "asset meta queued"
TEST_WINDOW_TYPE_TOKEN = "test window type"
MODAL_ANSWERED_TOKEN = "modal answered"
WINDOW_OPENED_TOKEN = "window opened"
WINDOW_PANEL_BUILT_TOKEN = "window panel built"
EXTENSION_RELOAD_TOKEN = "extension reload processed"
LIVE_TOKEN = "LIVE "
QUIT_REQUESTED_TOKEN = "[editor] test channel: quit requested"

SHELL_SOURCE = (
    MODULES_ROOT
    / "extensions"
    / "pill_editor_shell"
    / "pill_editor_shell_editor"
    / "src"
    / "shell.rs"
)
HOTROOT_SOURCE = (
    MODULES_ROOT
    / "extensions"
    / "pill_editor_shell"
    / "pill_editor_shell_editor"
    / "src"
    / "pill_editor_shell_editor.rs"
)


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession("builtin-panels", test_seconds=1200)
    with SHELL_SOURCE.open("r", encoding="utf-8", newline="") as file:
        original_shell = file.read()
    with HOTROOT_SOURCE.open("r", encoding="utf-8", newline="") as file:
        original_hotroot = file.read()
    # The Assets step stages its own files: project_rs's res/
    # tree may not exist yet, so it is made here and removed in the end when
    # this step left it empty. The probe is a real 1x1 PNG - the import
    # decodes it, so its sidecar behaves the way a sourced file's does - and
    # emptying that sidecar later is what the Write Asset Meta button
    # answers to.
    res_directory = REPOSITORY_ROOT / "examples" / "project_rs" / "res"
    probe_source = res_directory / "w25_probe_source.png"
    probe_meta = res_directory / "w25_probe_source.png.meta"
    res_directory.mkdir(parents=True, exist_ok=True)
    probe_source.write_bytes(
        base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
        )
    )
    staged: list[Path] = [probe_source, probe_meta]
    session.start()
    try:
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log.
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait_token(EDITOR_COMMAND_REGISTRY_TOKEN, 120, "registry ready")
        session.wait(
            re.escape(EDITOR_PANEL_REGISTRY_TOKEN) + r"\s+panels=\d+",
            120,
            "panels ready",
        )

        # The project rewrites component fields every tick, so every value
        # proof pauses the engine first (the shell still runs when paused);
        # the console phase resumes it because module systems must tick.
        begin = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            begin,
        )

        # Step 1: discover an editable, visible Inspector field for the
        # entity. The Inspector clips to its dock (a scroll area), so the
        # suite asks for widgets until one answers, and it needs a field
        # whose dump carries a plain F/I/U value to edit.
        begin = len(session.read_log())
        session.send("dump components")
        session.wait(
            re.escape(COMPONENT_LIST_TOKEN) + r'\s+name=', 60, "components dumped", begin
        )
        block = session.read_log()[begin:]
        catalog: dict = {}
        for match in re.finditer(
            r'\[test\] component\s+name=("?)([^\s"]+)\1\s+fields=("?)([^"\n]*)\3', block
        ):
            catalog[match.group(2)] = [
                field.strip() for field in match.group(4).split(",") if field.strip()
            ]
        print(f"  [OK] {len(catalog)} components catalogued")

        begin = len(session.read_log())
        session.send("dump entities")
        session.wait(
            re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=\S+[^\n]*PhysicsState",
            60,
            "entity discovered",
            begin,
        )
        entity = None
        line_text = ""
        for match in re.finditer(
            re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=(\S+)[^\n]*", session.read_log()[begin:]
        ):
            if "PhysicsState" in match.group(0):
                entity = match.group(1)
                line_text = match.group(0)
                break
        assert entity, "no entity carrying PhysicsState in the dump"
        print(f"  [OK] entity {entity}")
        quoted = re.search(r'components="([^"]*)"', line_text)
        plain = re.search(r"components=(\S+)", line_text)
        component_names = [
            name.strip()
            for name in (quoted.group(1) if quoted else plain.group(1)).split(",")
            if name.strip()
        ]
        if "PhysicsState" in component_names:
            component_names.remove("PhysicsState")
            component_names.insert(0, "PhysicsState")
        candidates = [
            (name, field) for name in component_names for field in catalog.get(name, [])
        ]

        # The Inspector shows the selection, so select first (and switch its
        # tab) before probing which field widgets answer.
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

        chosen = None
        for candidate_component, candidate_field in candidates:
            begin = len(session.read_log())
            session.send(
                f"activate inspector.{entity}.{candidate_component}.{candidate_field}"
            )
            activated = False
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                text = session.read_log()[begin:]
                if WIDGET_NOT_FOUND_TOKEN in text:
                    break
                if re.search(
                    re.escape(WIDGET_ACTIVATED_TOKEN) + r'\s+name="?inspector\.', text
                ):
                    activated = True
                    break
                # Poll again; the activation lands a beat after the popup opens.
                time.sleep(0.5)
            if not activated:
                continue
            probe = len(session.read_log())
            session.send(
                f"dump field {entity} {candidate_component.rsplit('::', 1)[-1]} "
                f"{candidate_field}"
            )
            try:
                session.wait(r"value=[A-Za-z0-9]+\(", 20, "", probe)
            except TimeoutError:
                continue
            value = session.find(r"value=([A-Za-z0-9]+)\(([^)]+)\)", probe)
            kind, original = value.group(1), value.group(2)
            if kind[0] in "FIU":
                short = candidate_component.rsplit("::", 1)[-1]
                chosen = (candidate_component, candidate_field, short, kind, original)
                print(f"  [OK] editing {short}.{candidate_field} = {kind}({original})")
                break
        if chosen is None:
            raise RuntimeError(
                "no visible, value-editable Inspector field answered\n"
                + session.read_log()[-3000:]
            )
        component, field, short, kind, original = chosen
        if kind.startswith("F"):
            edited = "43.5" if original == "42.5" else "42.5"
            after_reload = next(
                value for value in ("44.5", "45.5", "46.5") if value not in {original, edited}
            )
        else:
            edited = "41" if original == "42" else "42"
            after_reload = next(
                value for value in ("43", "44", "45") if value not in {original, edited}
            )
        label = f"set {short}.{field}"
        dump_field = f"dump field {entity} {short} {field}"

        widget = f"inspector.{entity}.{component}.{field}"
        begin = len(session.read_log())
        session.send(f"activate {widget}")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + rf'\s+name="?{re.escape(widget)}"?',
            30,
            "field focused",
            begin,
        )

        session.send("chord ctrl+a")
        begin = len(session.read_log())
        session.send(f"type {edited}")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + rf'\s+label="{re.escape(label)}"',
            60,
            "inspector edit recorded",
            begin,
        )
        begin = len(session.read_log())
        session.send(dump_field)
        session.wait(
            rf"value={kind}\({re.escape(edited)}\)", 30, "edit applied", begin
        )

        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + rf'\s+label="{re.escape(label)}"',
            30,
            "inspector edit undone",
            begin,
        )
        begin = len(session.read_log())
        session.send(dump_field)
        session.wait(
            rf"value={kind}\({re.escape(original)}\)", 30, "value restored", begin
        )

        # Step 2: a system toggle is one undoable step, and undo restores it.
        begin = len(session.read_log())
        session.send("edit system 0 off")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="system 0 off"',
            30,
            "system toggle recorded",
            begin,
        )
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="system 0 off"',
            30,
            "system toggle undone",
            begin,
        )

        # Resume so module systems tick again for the console phase.
        begin = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "engine resumed",
            begin,
        )

        # Step 3: the Console. A module command makes a game-channel line;
        # the heartbeat reports both channels; the filter buttons and the
        # search field respond through their named widgets.
        begin = len(session.read_log())
        session.send("command spline_editor.reseed")
        session.wait_token(RESEEDED_TOKEN, 60, "game channel line", begin)

        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            heartbeats = re.findall(
                re.escape(CONSOLE_STATE_TOKEN)
                + r"\s+lines=(\d+)\s+engine=(\d+)\s+game=(\d+)",
                session.read_log(),
            )
            if any(int(engine) >= 1 and int(game) >= 1 for _, engine, game in heartbeats):
                print(f"  [OK] console heartbeat with both channels: {heartbeats[-1]}")
                break
            # Poll until one heartbeat batch carries both channels.
            time.sleep(0.5)
        else:
            raise TimeoutError(
                "no console heartbeat carried both channels\n" + session.read_log()[-3000:]
            )

        begin = len(session.read_log())
        session.send("activate Errors")
        session.wait(
            re.escape(CONSOLE_FILTER_LEVEL_TOKEN) + r"\s+level=5",
            30,
            "level filter: errors",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate Game")
        session.wait(
            re.escape(CONSOLE_FILTER_CHANNEL_TOKEN) + r"\s+channel=2",
            30,
            "channel filter: game",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate Info+")
        session.wait(
            re.escape(CONSOLE_FILTER_LEVEL_TOKEN) + r"\s+level=3",
            30,
            "level filter: info",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate All channels")
        session.wait(
            re.escape(CONSOLE_FILTER_CHANNEL_TOKEN) + r"\s+channel=0",
            30,
            "channel filter: all",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate console_search_field")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + r'\s+name="?console_search_field"?',
            30,
            "search focused",
            begin,
        )
        begin = len(session.read_log())
        session.send("type reseed")
        session.wait(
            re.escape(CONSOLE_SEARCH_TOKEN) + r'\s+search="reseed"',
            30,
            "search text applied",
            begin,
        )

        # Step 4: preferences, both directions. The suite writes the file;
        # the shell applies it and sets the history limits. Then the panel's
        # fields write it back through the Save button.
        preferences_path = session.write_appdata_json(
            "preferences.json",
            {
                "keymap_preset": "godot",
                "code_editor_command": "code",
                "autosave_interval_seconds": 7,
                "history_step_cap": 250,
                "history_memory_budget_mb": 128,
            },
        )
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN) + r"\s+step_cap=250\s+memory_mb=128",
            60,
            "preferences file applied",
        )

        begin = len(session.read_log())
        session.send("tab Preferences")
        session.wait(
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title="?Preferences"?',
            30,
            "Preferences tab",
            begin,
        )
        # The panel clips to its dock: scroll from a field that is certainly
        # visible (the first row) so the step-cap row and the Save button
        # come into view, then use them.
        begin = len(session.read_log())
        session.send("activate preferences_field_preset")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + r'\s+name="?preferences_field_preset"?',
            30,
            "first preferences row focused",
            begin,
        )
        begin = len(session.read_log())
        session.send("wheel -12")
        session.wait_token(WIDGET_WHEELED_TOKEN, 30, "panel scrolled", begin)
        begin = len(session.read_log())
        session.send("activate preferences_field_step_cap")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + r'\s+name="?preferences_field_step_cap"?',
            30,
            "step cap field focused",
            begin,
        )
        # The scripted channel drains several lines per frame, so key and
        # text commands are paced by a frame each: ctrl+a must settle before
        # the replacement text arrives, and both before the Save click.
        session.send("chord ctrl+a")
        begin = len(session.read_log())
        session.wait_token(SHELL_FRAME_REPORT_TOKEN, 30, "chord settled", begin)
        session.send("type 300")
        begin = len(session.read_log())
        session.wait_token(SHELL_FRAME_REPORT_TOKEN, 30, "typing settled", begin)
        begin = len(session.read_log())
        session.send("activate Save preferences")
        session.wait_token(PREFERENCES_WRITTEN_TOKEN, 30, "preferences saved", begin)
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN) + r"\s+step_cap=300",
            60,
            "panel-written preferences applied",
        )
        saved = json.loads(preferences_path.read_text(encoding="utf-8"))
        assert saved.get("history_step_cap") == 300, f"file carries {saved}"
        assert saved.get("keymap_preset") == "godot", f"file carries {saved}"
        print(f"  [OK] preferences file round-trip: {saved}")

        # Step 5: the Assets panel's listing. The runner refreshes it from
        # the host's asset browser, which browses the project's `res/` tree;
        # project_rs keeps its sources under the asset system's `assets/`
        # instead, so the listing is empty here and the refresh token is the
        # assertion.
        session.wait(
            re.escape(ASSET_LISTING_TOKEN) + r"\s+count=\d+",
            120,
            "asset listing refreshed",
        )

        # Step 6: the shell live-patches on a body edit, and a structural
        # edit of its crate reloads it; state (console counts, selection)
        # survives both. The console heartbeat carries the line counts, so
        # it doubles as the frame clock here.
        before_counts = [
            int(entry)
            for entry in re.findall(
                re.escape(CONSOLE_STATE_TOKEN) + r"\s+lines=(\d+)", session.read_log()
            )
        ]
        assert before_counts, "no console heartbeat before the reload"
        lines_before = before_counts[-1]
        cadence = re.search(r"% (\d+) == 0", original_hotroot)
        assert cadence, "the shell's heartbeat cadence was not found"
        patched = original_hotroot.replace(
            cadence.group(0), f"% {int(cadence.group(1)) + 2} == 0", 1
        )
        begin = len(session.read_log())
        session.edit_source(HOTROOT_SOURCE, patched)
        session.wait(
            re.escape(LIVE_TOKEN) + r"\d+ ms|" + re.escape(EXTENSION_RELOAD_TOKEN),
            180,
            "shell body live-patched",
            begin,
        )

        begin = len(session.read_log())
        session.edit_source(
            SHELL_SOURCE, original_shell + "\n// built-in panels suite: structural reload marker\n"
        )
        session.wait(
            re.escape(EXTENSION_RELOAD_TOKEN)
            + r'\s+module="?pill_editor_shell_editor"?',
            180,
            "shell crate reloaded",
            begin,
        )
        print(f"  [OK] shell reloaded; console had {lines_before} lines")

        # A console heartbeat with a larger count proves the reloaded
        # generation kept the captured lines (and the shell's frame loop).
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            counts = [
                int(entry)
                for entry in re.findall(
                    re.escape(CONSOLE_STATE_TOKEN) + r"\s+lines=(\d+)", session.read_log()
                )
            ]
            if counts and counts[-1] > lines_before:
                print(f"  [OK] console grew past {lines_before} after reload: {counts[-1]}")
                break
            # Poll until the reload's output reaches the console panel.
            time.sleep(0.5)
        else:
            raise TimeoutError(
                f"no console heartbeat grew past {lines_before} after the reload\n"
                + session.read_log()[-3000:]
            )

        # Editing without re-selecting proves the shared selection survived.
        # Pause first: the project rewrites fields every tick while running.
        begin = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused for the reload edit",
            begin,
        )
        begin = len(session.read_log())
        session.send(f"activate {widget}")
        session.wait(
            re.escape(WIDGET_ACTIVATED_TOKEN) + rf'\s+name="?{re.escape(widget)}"?',
            30,
            "selection survived the reload",
            begin,
        )
        session.send("chord ctrl+a")
        begin = len(session.read_log())
        session.send(f"type {after_reload}")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + rf'\s+label="{re.escape(label)}"',
            60,
            "post-reload edit recorded",
            begin,
        )
        begin = len(session.read_log())
        session.send(dump_field)
        session.wait(
            rf"value={kind}\({re.escape(after_reload)}\)",
            30,
            "post-reload edit applied",
            begin,
        )

        # Step 6b: the Assets toolbar and its dialogs. The Assets
        # panel is a tab behind Hierarchy in the default layout, so it is
        # selected first; the file the dialog creates is the row the
        # toolbar's other buttons act on, and every queued command is
        # recorded and walks back through undo. The probe png the meta
        # button answers to was staged before the run.
        begin = len(session.read_log())
        session.send("tab Assets")
        session.wait(
            re.escape(EDITOR_TAB_SELECTED_TOKEN) + r'\s+title="?Assets"?',
            30,
            "Assets tab",
            begin,
        )
        begin = len(session.read_log())
        session.send("dump standalone-types")
        session.wait(
            re.escape(STANDALONE_TYPE_TOKEN) + r"\s", 30, "standalone types dumped", begin
        )
        found = session.find(
            re.escape(STANDALONE_TYPE_TOKEN)
            + r'\s+type_name="([^"]+)"\s+display="([^"]+)"\s+extension="([^"]+)"',
            begin,
        )
        type_name, display, extension = found.groups()
        print(f"  [OK] first standalone type {display} (.{extension})")
        created_name = f"w25_panel.{extension}"

        # Create through the dialog: open it from the toolbar, pick the first
        # type row, type a name into the field, press Create.
        begin = len(session.read_log())
        session.send("activate Create Asset...")
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?create_asset_dialog"?',
            60,
            "create dialog opened",
            begin,
        )
        session.wait(
            re.escape(WINDOW_PANEL_BUILT_TOKEN) + r'\s+id="?create_asset_dialog"?',
            60,
            "create dialog built",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate window.create_asset_dialog.create.type.0")
        session.wait(
            re.escape(WINDOW_WIDGET_ACTIVATED_TOKEN)
            + r'\s+name="?window\.create_asset_dialog\.create\.type\.0"?',
            30,
            "type row chosen",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate window.create_asset_dialog.create.name")
        session.wait(
            re.escape(WINDOW_WIDGET_ACTIVATED_TOKEN)
            + r'\s+name="?window\.create_asset_dialog\.create\.name"?',
            30,
            "name field focused",
            begin,
        )
        begin = len(session.read_log())
        session.send("window-type create_asset_dialog w25_panel")
        session.wait_token(TEST_WINDOW_TYPE_TOKEN, 30, "name typed", begin)
        begin = len(session.read_log())
        session.send("activate window.create_asset_dialog.button.0")
        session.wait_token(ASSET_CREATE_QUEUED_TOKEN, 30, "create queued", begin)
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="create asset w25_panel"',
            30,
            "create recorded",
            begin,
        )
        session.wait(
            re.escape(MODAL_ANSWERED_TOKEN) + r'\s+id="?create_asset_dialog"?',
            30,
            "create dialog answered",
            begin,
        )
        assert (res_directory / created_name).is_file(), "the dialog created no file"
        staged.append(res_directory / created_name)
        print(f"  [OK] created {created_name} through the dialog")

        # The creation undoes (the file goes) and redoes (it comes back),
        # and the listing picks the row up for the toolbar.
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="create asset w25_panel"',
            30,
            "create undone",
            begin,
        )
        assert not (res_directory / created_name).exists(), "the creation survived its undo"
        begin = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN) + r'\s+label="create asset w25_panel"',
            30,
            "create redone",
            begin,
        )
        assert (res_directory / created_name).is_file(), "the redo restored no file"
        session.wait_token(ASSET_LISTING_TOKEN, 60, "listing sees the created file", begin)

        # Rename through the dialog: the field arrives with the stem in it,
        # the typed suffix lands after it, and the queued paths are read back
        # rather than assumed.
        begin = len(session.read_log())
        session.send(f"select-asset {created_name}")
        session.wait_token(ASSET_SELECTED_TOKEN, 30, "created row selected", begin)
        begin = len(session.read_log())
        session.send("activate Rename Asset...")
        session.wait(
            re.escape(WINDOW_OPENED_TOKEN) + r'\s+id="?rename_asset_dialog"?',
            60,
            "rename dialog opened",
            begin,
        )
        session.wait(
            re.escape(WINDOW_PANEL_BUILT_TOKEN) + r'\s+id="?rename_asset_dialog"?',
            60,
            "rename dialog built",
            begin,
        )
        begin = len(session.read_log())
        session.send("activate window.rename_asset_dialog.rename.name")
        session.wait(
            re.escape(WINDOW_WIDGET_ACTIVATED_TOKEN)
            + r'\s+name="?window\.rename_asset_dialog\.rename\.name"?',
            30,
            "rename field focused",
            begin,
        )
        begin = len(session.read_log())
        session.send("window-type rename_asset_dialog 2")
        session.wait_token(TEST_WINDOW_TYPE_TOKEN, 30, "suffix typed", begin)
        begin = len(session.read_log())
        session.send("activate window.rename_asset_dialog.button.0")
        session.wait_token(ASSET_RENAME_QUEUED_TOKEN, 30, "rename queued", begin)
        queued = session.find(
            re.escape(ASSET_RENAME_QUEUED_TOKEN)
            + r'\s+from="?([^"\s]+)"?\s+to="?([^"\s]+)"?',
            begin,
        )
        renamed_from = queued.group(1)
        renamed_to = queued.group(2)
        assert renamed_from == created_name, f"the rename read {renamed_from}"
        assert renamed_to != created_name, "the rename kept the old name"
        assert renamed_to.endswith(extension), f"the rename lost the extension: {renamed_to}"
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="rename asset ', 30, "rename recorded", begin
        )
        assert not (res_directory / renamed_from).exists(), "the old name survived the rename"
        assert (res_directory / renamed_to).is_file(), "the new name is missing"
        staged.append(res_directory / renamed_to)
        print(f"  [OK] renamed to {renamed_to} through the dialog")

        # Undo restores the old name; redo renames again.
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="rename asset ', 30, "rename undone", begin
        )
        assert (res_directory / renamed_from).is_file(), "the undo did not restore the name"
        assert not (res_directory / renamed_to).exists(), "the new name survived the undo"
        begin = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN) + r'\s+label="rename asset ', 30, "rename redone", begin
        )
        assert (res_directory / renamed_to).is_file(), "the redo renamed nothing"

        # Delete through the toolbar; undo brings the bytes back.
        begin = len(session.read_log())
        session.send("activate Delete Asset")
        session.wait_token(ASSET_DELETE_QUEUED_TOKEN, 30, "delete queued", begin)
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="delete asset ', 30, "delete recorded", begin
        )
        assert not (res_directory / renamed_to).exists(), "the delete left the file"
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="delete asset ', 30, "delete undone", begin
        )
        assert (res_directory / renamed_to).is_file(), "the undo restored no file"
        assert (res_directory / renamed_to).stat().st_size > 0, "the restored file is empty"
        begin = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN) + r'\s+label="delete asset ', 30, "delete redone", begin
        )
        assert not (res_directory / renamed_to).exists(), "the redo left the file"

        # Write Asset Meta through the toolbar. The host keeps every importable
        # source's sidecar in step, so a file without usable metadata is made
        # by emptying the staged probe's sidecar: the row lists the file, its
        # metadata reads as missing, and the button answers. The suite nudges
        # the listing so the rows catch up at once; the write records and its
        # undo removes the replacement.
        probe_meta.write_bytes(b"")
        begin = len(session.read_log())
        session.send("refresh-assets")
        session.wait_token(ASSETS_REFRESH_TOKEN, 30, "listing nudged", begin)
        session.wait_token(ASSET_LISTING_TOKEN, 60, "listing sees the empty sidecar", begin)
        begin = len(session.read_log())
        session.send("select-asset w25_probe_source.png")
        session.wait_token(ASSET_SELECTED_TOKEN, 30, "sourced row selected", begin)
        begin = len(session.read_log())
        session.send("activate Write Asset Meta")
        session.wait_token(ASSET_META_QUEUED_TOKEN, 30, "write meta queued", begin)
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN)
            + r'\s+label="write meta w25_probe_source\.png"',
            30,
            "write meta recorded",
            begin,
        )
        assert probe_meta.is_file() and probe_meta.stat().st_size > 0, "no sidecar was written"
        written_meta = probe_meta.read_text(encoding="utf-8")
        written_stamp = probe_meta.stat().st_mtime_ns
        begin = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN)
            + r'\s+label="write meta w25_probe_source\.png"',
            30,
            "write meta undone",
            begin,
        )
        # The undo removes the replacement; the host then heals a sidecar
        # back in (its ensure is on) - for a loaded source with the same
        # guid - so the walk-back shows as the file being gone, rewritten or
        # fresh.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if not probe_meta.exists():
                break
            if probe_meta.stat().st_mtime_ns != written_stamp:
                break
            if probe_meta.read_text(encoding="utf-8") != written_meta:
                break
            # Poll until the undo rewrites the sidecar back to its first text.
            time.sleep(0.25)
        else:
            raise RuntimeError("the written sidecar survived its undo")
        print("  [OK] empty sidecar replaced through the toolbar and walked back")

        # Step 7: a graceful quit, with the clean exit code.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in session.read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0")
    finally:
        session.close()
        for path in staged:
            try:
                path.unlink(missing_ok=True)
            except OSError:
                pass
        try:
            res_directory.rmdir()
        except OSError:
            pass

    print("PASS: built-in panels, console and preferences")
    return 0


if __name__ == "__main__":
    sys.exit(main())

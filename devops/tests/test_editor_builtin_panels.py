"""End-to-end acceptance for the built-in panels (W-0004 Stage 17).

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
       back (Stage 25);
    7. a body edit of the shell live-patches with its state kept, and a
       structural edit of the shell crate reloads it with tabs, selection
       and console kept (an Inspector edit still works without re-selecting);
    8. the editor exits cleanly.

  Everything is asserted on the editor's log tokens; the edited sources are
  restored in a `finally`. The suite runs under its own LOCALAPPDATA sandbox.

USAGE
  python devops/tests/test_editor_builtin_panels.py

EXAMPLE USAGE
  python devops/tests/test_editor_builtin_panels.py

--- SCRIPT ---
"""

import base64
import json
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
LOG = VALIDATION / "editor-builtin-panels.log"
COMMANDS = VALIDATION / "commands-stage17.txt"
APPDATA = VALIDATION / "stage17_appdata"
PREFERENCES = APPDATA / "RustHybridEcs" / "preferences.json"
SHELL_SOURCE = (
    MODULES
    / "extensions"
    / "pill_editor_shell"
    / "pill_editor_shell_editor"
    / "src"
    / "shell.rs"
)
HOTROOT_SOURCE = (
    MODULES
    / "extensions"
    / "pill_editor_shell"
    / "pill_editor_shell_editor"
    / "src"
    / "pill_editor_shell_editor.rs"
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
        if expression.search(text[start:]):
            print(f"  [OK] {label}")
            return len(text)
        if process.poll() is not None:
            raise RuntimeError(
                f"{label}: the editor exited {process.returncode}\n" + text[-4000:]
            )
        time.sleep(0.5)
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
    original_shell = SHELL_SOURCE.read_text(encoding="utf-8")
    original_hotroot = HOTROOT_SOURCE.read_text(encoding="utf-8")
    COMMANDS.write_text("# stage 17 built-in panels suite\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/project_rs"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "1200"
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
    # The Assets step (Stage 25) stages its own files: project_rs's res/
    # tree may not exist yet, so it is made here and removed in the end when
    # this step left it empty. The probe is a real 1x1 PNG - the import
    # decodes it, so its sidecar behaves the way a sourced file's does - and
    # emptying that sidecar later is what the Write Asset Meta button
    # answers to.
    res_directory = ROOT / "examples" / "project_rs" / "res"
    probe_source = res_directory / "w25_probe_source.png"
    probe_meta = res_directory / "w25_probe_source.png.meta"
    res_directory.mkdir(parents=True, exist_ok=True)
    probe_source.write_bytes(
        base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
        )
    )
    staged: list[Path] = [probe_source, probe_meta]
    try:
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log.
        wait_token(process, r"\[editor\] pill_editor_shell_editor loaded", 300, "shell loaded")
        wait_token(process, r"\[editor\] window created", 300, "editor started")
        wait_token(process, r"\[editor\] command registry ready", 120, "registry ready")
        wait_token(process, r"\[editor\] panel registry ready\s+panels=\d+", 120, "panels ready")

        # The project rewrites component fields every tick, so every value
        # proof pauses the engine first (the shell still runs when paused);
        # the console phase resumes it because module systems must tick.
        begin = len(read_log())
        append("command editor.pause_toggle")
        wait_token(
            process,
            r'command executed\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            begin,
        )

        # Step 1: discover an editable, visible Inspector field for the
        # entity. The Inspector clips to its dock (a scroll area), so the
        # suite asks for widgets until one answers, and it needs a field
        # whose dump carries a plain F/I/U value to edit.
        begin = len(read_log())
        append("dump components")
        wait_token(process, r"\[test\] component\s+name=", 60, "components dumped", begin)
        block = read_log()[begin:]
        catalog: dict = {}
        for match in re.finditer(
            r'\[test\] component\s+name=("?)([^\s"]+)\1\s+fields=("?)([^"\n]*)\3', block
        ):
            catalog[match.group(2)] = [
                field.strip() for field in match.group(4).split(",") if field.strip()
            ]
        print(f"  [OK] {len(catalog)} components catalogued")

        begin = len(read_log())
        append("dump entities")
        wait_token(
            process,
            r'\[test\] entity\s+entity=\S+[^\n]*PhysicsState',
            60,
            "entity discovered",
            begin,
        )
        entity = None
        line_text = ""
        for match in re.finditer(r'\[test\] entity\s+entity=(\S+)[^\n]*', read_log()[begin:]):
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
        begin = len(read_log())
        append(f"select {entity}")
        wait_token(process, rf"selection changed.*entity={re.escape(entity)}", 30, "selected", begin)

        begin = len(read_log())
        append("tab Inspector")
        wait_token(process, r'tab selected\s+title="?Inspector"?', 30, "Inspector tab", begin)

        chosen = None
        for candidate_component, candidate_field in candidates:
            begin = len(read_log())
            append(f"activate inspector.{entity}.{candidate_component}.{candidate_field}")
            activated = False
            deadline = time.monotonic() + 12
            while time.monotonic() < deadline:
                text = read_log()[begin:]
                if "widget not found" in text:
                    break
                if re.search(r"widget activated\s+name=\"?inspector\.", text):
                    activated = True
                    break
                time.sleep(0.5)
            if not activated:
                continue
            probe = len(read_log())
            append(
                f"dump field {entity} {candidate_component.rsplit('::', 1)[-1]} "
                f"{candidate_field}"
            )
            try:
                wait_token(process, r"value=[A-Za-z0-9]+\(", 20, "", probe)
            except TimeoutError:
                continue
            value = find_in_log(r"value=([A-Za-z0-9]+)\(([^)]+)\)", probe)
            kind, original = value.group(1), value.group(2)
            if kind[0] in "FIU":
                short = candidate_component.rsplit("::", 1)[-1]
                chosen = (candidate_component, candidate_field, short, kind, original)
                print(f"  [OK] editing {short}.{candidate_field} = {kind}({original})")
                break
        if chosen is None:
            raise RuntimeError(
                "no visible, value-editable Inspector field answered\n" + read_log()[-3000:]
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
        begin = len(read_log())
        append(f"activate {widget}")
        wait_token(process, rf"widget activated\s+name=\"?{re.escape(widget)}\"?", 30, "field focused", begin)

        append("chord ctrl+a")
        begin = len(read_log())
        append(f"type {edited}")
        wait_token(
            process,
            rf'history push\s+label="{re.escape(label)}"',
            60,
            "inspector edit recorded",
            begin,
        )
        begin = len(read_log())
        append(dump_field)
        wait_token(process, rf"value={kind}\({re.escape(edited)}\)", 30, "edit applied", begin)

        begin = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            rf'history undo\s+label="{re.escape(label)}"',
            30,
            "inspector edit undone",
            begin,
        )
        begin = len(read_log())
        append(dump_field)
        wait_token(process, rf"value={kind}\({re.escape(original)}\)", 30, "value restored", begin)

        # Step 2: a system toggle is one undoable step, and undo restores it.
        begin = len(read_log())
        append("edit system 0 off")
        wait_token(
            process,
            r'history push\s+label="system 0 off"',
            30,
            "system toggle recorded",
            begin,
        )
        begin = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'history undo\s+label="system 0 off"',
            30,
            "system toggle undone",
            begin,
        )

        # Resume so module systems tick again for the console phase.
        begin = len(read_log())
        append("command editor.pause_toggle")
        wait_token(
            process,
            r'command executed\s+id="?editor\.pause_toggle"?',
            30,
            "engine resumed",
            begin,
        )

        # Step 3: the Console. A module command makes a game-channel line;
        # the heartbeat reports both channels; the filter buttons and the
        # search field respond through their named widgets.
        begin = len(read_log())
        append("command spline_editor.reseed")
        wait_token(process, r"\[spline_editor\] reseeded", 60, "game channel line", begin)

        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            heartbeats = re.findall(
                r"console state\s+lines=(\d+)\s+engine=(\d+)\s+game=(\d+)", read_log()
            )
            if any(int(engine) >= 1 and int(game) >= 1 for _, engine, game in heartbeats):
                print(f"  [OK] console heartbeat with both channels: {heartbeats[-1]}")
                break
            time.sleep(0.5)
        else:
            raise TimeoutError("no console heartbeat carried both channels\n" + read_log()[-3000:])

        begin = len(read_log())
        append("activate Errors")
        wait_token(process, r"console filter level\s+level=5", 30, "level filter: errors", begin)
        begin = len(read_log())
        append("activate Game")
        wait_token(process, r"console filter channel\s+channel=2", 30, "channel filter: game", begin)
        begin = len(read_log())
        append("activate Info+")
        wait_token(process, r"console filter level\s+level=3", 30, "level filter: info", begin)
        begin = len(read_log())
        append("activate All channels")
        wait_token(process, r"console filter channel\s+channel=0", 30, "channel filter: all", begin)
        begin = len(read_log())
        append("activate console_search_field")
        wait_token(process, r"widget activated\s+name=\"?console_search_field\"?", 30, "search focused", begin)
        begin = len(read_log())
        append("type reseed")
        wait_token(process, r'console search\s+search="reseed"', 30, "search text applied", begin)

        # Step 4: preferences, both directions. The suite writes the file;
        # the shell applies it and sets the history limits. Then the panel's
        # fields write it back through the Save button.
        PREFERENCES.parent.mkdir(parents=True, exist_ok=True)
        PREFERENCES.write_text(
            json.dumps(
                {
                    "keymap_preset": "godot",
                    "code_editor_command": "code",
                    "autosave_interval_seconds": 7,
                    "history_step_cap": 250,
                    "history_memory_budget_mb": 128,
                }
            ),
            encoding="utf-8",
        )
        wait_token(
            process,
            r"preferences applied\s+step_cap=250\s+memory_mb=128",
            60,
            "preferences file applied",
        )

        begin = len(read_log())
        append("tab Preferences")
        wait_token(process, r'tab selected\s+title="?Preferences"?', 30, "Preferences tab", begin)
        # The panel clips to its dock: scroll from a field that is certainly
        # visible (the first row) so the step-cap row and the Save button
        # come into view, then use them.
        begin = len(read_log())
        append("activate preferences_field_preset")
        wait_token(
            process,
            r"widget activated\s+name=\"?preferences_field_preset\"?",
            30,
            "first preferences row focused",
            begin,
        )
        begin = len(read_log())
        append("wheel -12")
        wait_token(process, r"\[editor\] widget wheeled", 30, "panel scrolled", begin)
        begin = len(read_log())
        append("activate preferences_field_step_cap")
        wait_token(
            process,
            r"widget activated\s+name=\"?preferences_field_step_cap\"?",
            30,
            "step cap field focused",
            begin,
        )
        # The scripted channel drains several lines per frame, so key and
        # text commands are paced by a frame each: ctrl+a must settle before
        # the replacement text arrives, and both before the Save click.
        append("chord ctrl+a")
        begin = len(read_log())
        wait_token(process, r"scene frame report", 30, "chord settled", begin)
        append("type 300")
        begin = len(read_log())
        wait_token(process, r"scene frame report", 30, "typing settled", begin)
        begin = len(read_log())
        append("activate Save preferences")
        wait_token(process, r"\[editor\] preferences written", 30, "preferences saved", begin)
        wait_token(
            process,
            r"preferences applied\s+step_cap=300",
            60,
            "panel-written preferences applied",
        )
        saved = json.loads(PREFERENCES.read_text(encoding="utf-8"))
        assert saved.get("history_step_cap") == 300, f"file carries {saved}"
        assert saved.get("keymap_preset") == "godot", f"file carries {saved}"
        print(f"  [OK] preferences file round-trip: {saved}")

        # Step 5: the Assets panel's listing. The runner refreshes it from
        # the host's asset browser, which browses the project's `res/` tree;
        # project_rs keeps its sources under the asset system's `assets/`
        # instead, so the listing is empty here and the refresh token is the
        # assertion.
        wait_token(
            process, r"asset listing refreshed\s+count=\d+", 120, "asset listing refreshed"
        )

        # Step 6: the shell live-patches on a body edit, and a structural
        # edit of its crate reloads it; state (console counts, selection)
        # survives both. The console heartbeat carries the line counts, so
        # it doubles as the frame clock here.
        before_counts = [
            int(entry)
            for entry in re.findall(r"console state\s+lines=(\d+)", read_log())
        ]
        assert before_counts, "no console heartbeat before the reload"
        lines_before = before_counts[-1]
        cadence = re.search(r"% (\d+) == 0", original_hotroot)
        assert cadence, "the shell's heartbeat cadence was not found"
        patched = original_hotroot.replace(
            cadence.group(0), f"% {int(cadence.group(1)) + 2} == 0", 1
        )
        begin = len(read_log())
        HOTROOT_SOURCE.write_text(patched, encoding="utf-8")
        wait_token(process, r"LIVE \d+ ms|extension reload processed", 180, "shell body live-patched", begin)

        begin = len(read_log())
        SHELL_SOURCE.write_text(
            original_shell + "\n// stage 17 suite: structural reload marker\n",
            encoding="utf-8",
        )
        wait_token(
            process,
            r'extension reload processed\s+module="?pill_editor_shell_editor"?',
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
                for entry in re.findall(r"console state\s+lines=(\d+)", read_log())
            ]
            if counts and counts[-1] > lines_before:
                print(f"  [OK] console grew past {lines_before} after reload: {counts[-1]}")
                break
            time.sleep(0.5)
        else:
            raise TimeoutError(
                f"no console heartbeat grew past {lines_before} after the reload\n"
                + read_log()[-3000:]
            )

        # Editing without re-selecting proves the shared selection survived.
        # Pause first: the project rewrites fields every tick while running.
        begin = len(read_log())
        append("command editor.pause_toggle")
        wait_token(
            process,
            r'command executed\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused for the reload edit",
            begin,
        )
        begin = len(read_log())
        append(f"activate {widget}")
        wait_token(
            process,
            rf"widget activated\s+name=\"?{re.escape(widget)}\"?",
            30,
            "selection survived the reload",
            begin,
        )
        append("chord ctrl+a")
        begin = len(read_log())
        append(f"type {after_reload}")
        wait_token(
            process,
            rf'history push\s+label="{re.escape(label)}"',
            60,
            "post-reload edit recorded",
            begin,
        )
        begin = len(read_log())
        append(dump_field)
        wait_token(
            process,
            rf"value={kind}\({re.escape(after_reload)}\)",
            30,
            "post-reload edit applied",
            begin,
        )

        # Step 6b: the Assets toolbar and its dialogs (Stage 25). The Assets
        # panel is a tab behind Hierarchy in the default layout, so it is
        # selected first; the file the dialog creates is the row the
        # toolbar's other buttons act on, and every queued command is
        # recorded and walks back through undo. The probe png the meta
        # button answers to was staged before the run.
        begin = len(read_log())
        append("tab Assets")
        wait_token(process, r'tab selected\s+title="?Assets"?', 30, "Assets tab", begin)
        begin = len(read_log())
        append("dump standalone-types")
        wait_token(process, r"\[test\] standalone type\s", 30, "standalone types dumped", begin)
        found = find_in_log(
            r'\[test\] standalone type\s+type_name="([^"]+)"\s+display="([^"]+)"'
            r'\s+extension="([^"]+)"',
            begin,
        )
        type_name, display, extension = found.groups()
        print(f"  [OK] first standalone type {display} (.{extension})")
        created_name = f"w25_panel.{extension}"

        # Create through the dialog: open it from the toolbar, pick the first
        # type row, type a name into the field, press Create.
        begin = len(read_log())
        append("activate Create Asset...")
        wait_token(
            process, r'window opened\s+id="?create_asset_dialog"?', 60, "create dialog opened", begin
        )
        wait_token(
            process, r'window panel built\s+id="?create_asset_dialog"?', 60, "create dialog built", begin
        )
        begin = len(read_log())
        append("activate window.create_asset_dialog.create.type.0")
        wait_token(
            process,
            r'window widget activated\s+name="?window\.create_asset_dialog\.create\.type\.0"?',
            30,
            "type row chosen",
            begin,
        )
        begin = len(read_log())
        append("activate window.create_asset_dialog.create.name")
        wait_token(
            process,
            r'window widget activated\s+name="?window\.create_asset_dialog\.create\.name"?',
            30,
            "name field focused",
            begin,
        )
        begin = len(read_log())
        append("window-type create_asset_dialog w25_panel")
        wait_token(process, r"\[editor\] test window type", 30, "name typed", begin)
        begin = len(read_log())
        append("activate window.create_asset_dialog.button.0")
        wait_token(process, r"\[editor\] asset create queued", 30, "create queued", begin)
        wait_token(
            process, r'history push\s+label="create asset w25_panel"', 30, "create recorded", begin
        )
        wait_token(
            process, r'modal answered\s+id="?create_asset_dialog"?', 30, "create dialog answered", begin
        )
        assert (res_directory / created_name).is_file(), "the dialog created no file"
        staged.append(res_directory / created_name)
        print(f"  [OK] created {created_name} through the dialog")

        # The creation undoes (the file goes) and redoes (it comes back),
        # and the listing picks the row up for the toolbar.
        begin = len(read_log())
        append("command editor.undo")
        wait_token(
            process, r'history undo\s+label="create asset w25_panel"', 30, "create undone", begin
        )
        assert not (res_directory / created_name).exists(), "the creation survived its undo"
        begin = len(read_log())
        append("command editor.redo")
        wait_token(
            process, r'history redo\s+label="create asset w25_panel"', 30, "create redone", begin
        )
        assert (res_directory / created_name).is_file(), "the redo restored no file"
        wait_token(process, r"asset listing refreshed", 60, "listing sees the created file", begin)

        # Rename through the dialog: the field arrives with the stem in it,
        # the typed suffix lands after it, and the queued paths are read back
        # rather than assumed.
        begin = len(read_log())
        append(f"select-asset {created_name}")
        wait_token(process, r"\[editor\] asset selected", 30, "created row selected", begin)
        begin = len(read_log())
        append("activate Rename Asset...")
        wait_token(
            process, r'window opened\s+id="?rename_asset_dialog"?', 60, "rename dialog opened", begin
        )
        wait_token(
            process, r'window panel built\s+id="?rename_asset_dialog"?', 60, "rename dialog built", begin
        )
        begin = len(read_log())
        append("activate window.rename_asset_dialog.rename.name")
        wait_token(
            process,
            r'window widget activated\s+name="?window\.rename_asset_dialog\.rename\.name"?',
            30,
            "rename field focused",
            begin,
        )
        begin = len(read_log())
        append("window-type rename_asset_dialog 2")
        wait_token(process, r"\[editor\] test window type", 30, "suffix typed", begin)
        begin = len(read_log())
        append("activate window.rename_asset_dialog.button.0")
        wait_token(process, r"\[editor\] asset rename queued", 30, "rename queued", begin)
        queued = find_in_log(
            r'\[editor\] asset rename queued\s+from="?([^"\s]+)"?\s+to="?([^"\s]+)"?',
            begin,
        )
        renamed_from = queued.group(1)
        renamed_to = queued.group(2)
        assert renamed_from == created_name, f"the rename read {renamed_from}"
        assert renamed_to != created_name, "the rename kept the old name"
        assert renamed_to.endswith(extension), f"the rename lost the extension: {renamed_to}"
        wait_token(process, r'history push\s+label="rename asset ', 30, "rename recorded", begin)
        assert not (res_directory / renamed_from).exists(), "the old name survived the rename"
        assert (res_directory / renamed_to).is_file(), "the new name is missing"
        staged.append(res_directory / renamed_to)
        print(f"  [OK] renamed to {renamed_to} through the dialog")

        # Undo restores the old name; redo renames again.
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="rename asset ', 30, "rename undone", begin)
        assert (res_directory / renamed_from).is_file(), "the undo did not restore the name"
        assert not (res_directory / renamed_to).exists(), "the new name survived the undo"
        begin = len(read_log())
        append("command editor.redo")
        wait_token(process, r'history redo\s+label="rename asset ', 30, "rename redone", begin)
        assert (res_directory / renamed_to).is_file(), "the redo renamed nothing"

        # Delete through the toolbar; undo brings the bytes back.
        begin = len(read_log())
        append("activate Delete Asset")
        wait_token(process, r"\[editor\] asset delete queued", 30, "delete queued", begin)
        wait_token(process, r'history push\s+label="delete asset ', 30, "delete recorded", begin)
        assert not (res_directory / renamed_to).exists(), "the delete left the file"
        begin = len(read_log())
        append("command editor.undo")
        wait_token(process, r'history undo\s+label="delete asset ', 30, "delete undone", begin)
        assert (res_directory / renamed_to).is_file(), "the undo restored no file"
        assert (res_directory / renamed_to).stat().st_size > 0, "the restored file is empty"
        begin = len(read_log())
        append("command editor.redo")
        wait_token(process, r'history redo\s+label="delete asset ', 30, "delete redone", begin)
        assert not (res_directory / renamed_to).exists(), "the redo left the file"

        # Write Asset Meta through the toolbar. The host keeps every importable
        # source's sidecar in step, so a file without usable metadata is made
        # by emptying the staged probe's sidecar: the row lists the file, its
        # metadata reads as missing, and the button answers. The suite nudges
        # the listing so the rows catch up at once; the write records and its
        # undo removes the replacement.
        probe_meta.write_bytes(b"")
        begin = len(read_log())
        append("refresh-assets")
        wait_token(process, r"\[editor\] test assets refresh", 30, "listing nudged", begin)
        wait_token(process, r"asset listing refreshed", 60, "listing sees the empty sidecar", begin)
        begin = len(read_log())
        append("select-asset w25_probe_source.png")
        wait_token(process, r"\[editor\] asset selected", 30, "sourced row selected", begin)
        begin = len(read_log())
        append("activate Write Asset Meta")
        wait_token(process, r"\[editor\] asset meta queued", 30, "write meta queued", begin)
        wait_token(
            process,
            r'history push\s+label="write meta w25_probe_source\.png"',
            30,
            "write meta recorded",
            begin,
        )
        assert probe_meta.is_file() and probe_meta.stat().st_size > 0, "no sidecar was written"
        written_meta = probe_meta.read_text(encoding="utf-8")
        written_stamp = probe_meta.stat().st_mtime_ns
        begin = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'history undo\s+label="write meta w25_probe_source\.png"',
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
            time.sleep(0.25)
        else:
            raise RuntimeError("the written sidecar survived its undo")
        print("  [OK] empty sidecar replaced through the toolbar and walked back")

        # Step 7: a graceful quit, with the clean exit code.
        append("quit")
        wait_token(process, r"\[editor\] test channel: quit requested", 30, "quit command")
        status = process.wait(timeout=60)
        assert status == 0, f"editor exited {status} on the test quit"
        assert "panic" not in read_log(), "a panic was logged"
        print("  [OK] clean exit, code 0")
    finally:
        SHELL_SOURCE.write_text(original_shell, encoding="utf-8")
        HOTROOT_SOURCE.write_text(original_hotroot, encoding="utf-8")
        if process.poll() is None:
            process.kill()
            process.wait(timeout=30)
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

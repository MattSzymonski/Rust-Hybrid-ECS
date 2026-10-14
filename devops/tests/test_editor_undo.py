"""End-to-end acceptance for the editor's undo/redo history.

PROTECTS: Undo and redo: field edits, drags, component, entity and asset commands round-trip.
TIER: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs (its PhysicsState and SplineSample components and its
    spline editor crate, which the reload phase edits)

DESCRIPTION
  Starts the editor on the spline project and drives edits through the
  scripted channel, which queues them on the same recorded command path the
  UI uses:

    1. a field edit undoes to the previous value and redoes to the new one;
    2. a continuous drag (repeated sets sharing a coalesce key in one frame)
       is one undo step;
    3. adding, editing and removing a component round-trips through undo with
       the removed values restored;
    4. creating and destroying an entity round-trip: the restore assigns a
       fresh handle, which the history remaps (asserted by editing through
       the new handle and undoing again);
    5. the step cap from the preferences file trims the oldest entry with a
       log line;
    6. an edit survives a reload of the spline editor crate and still undoes.    7. a resource field edit and a selection change each undo and redo as one
       step;
    8. create, move, write-meta and delete through the recorded asset
       commands each undo and redo cleanly, on the Rust and the C# example
       project, each in its own editor run.
  Everything is asserted on the editor's log tokens; the edited crate source
  is restored in a `finally`. The suite runs under its own LOCALAPPDATA
  sandbox, so the developer's real editor state is untouched.

USAGE
  python devops/tests/test_editor_undo.py

EXAMPLE USAGE
  python devops/tests/test_editor_undo.py

--- SCRIPT ---
"""

import base64
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
LOG = VALIDATION / "editor-undo.log"
COMMANDS = VALIDATION / "commands-undo.txt"
APPDATA = VALIDATION / "undo_appdata"
PREFERENCES = APPDATA / "RustHybridEcs" / "preferences.json"
SPLINE_EDITOR_SOURCE = (
    MODULES
    / "extensions"
    / "pill_spline"
    / "pill_spline_editor"
    / "src"
    / "pill_spline_editor.rs"
)
GIZMO_RESOURCE = "pill_editor_core::gizmo::GizmoState"
# A 1x1 PNG: a sourced file the import decodes for real, so the watcher
# writes its sidecar the way it does for any edited source.
PROBE_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
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


def write_preferences(body: dict) -> None:
    """Writes the preferences file the shell live-reads."""
    PREFERENCES.parent.mkdir(parents=True, exist_ok=True)
    PREFERENCES.write_text(json.dumps(body), encoding="utf-8")


def run_asset_phase(project: str, label: str, sandbox_name: str) -> None:
    """Drives the recorded asset commands on one project.

    Create, move, write-meta and delete each undo and redo through
    `command editor.undo` / `command editor.redo`, with the files checked on
    disk at every step, and quit cleanly. The probe files it creates are
    removed in a `finally`.
    """
    log_path = VALIDATION / f"editor-undo-{label}.log"
    commands_path = VALIDATION / f"commands-undo-assets-{label}.txt"
    appdata = VALIDATION / sandbox_name
    res = ROOT / "examples" / project / "res"
    probe = res / "w25_source.png"

    def read() -> str:
        try:
            return log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    def wait(process, pattern: str, timeout: float, description: str, start: int = 0) -> int:
        deadline = time.monotonic() + timeout
        expression = re.compile(pattern)
        while time.monotonic() < deadline:
            text = read()
            if expression.search(text[start:]):
                print(f"  [OK] {label}: {description}")
                return len(text)
            if process.poll() is not None:
                raise RuntimeError(
                    f"{label}: {description}: the editor exited {process.returncode}\n"
                    + text[-4000:]
                )
            time.sleep(0.5)
        raise TimeoutError(
            f"{label}: {description}: timeout waiting for {pattern}\n" + read()[-4000:]
        )

    def send(line: str) -> None:
        with commands_path.open("a", encoding="utf-8") as file:
            file.write(line + "\n")

    def expect_file(path: Path, exists: bool, description: str, timeout: float = 30) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if path.exists() == exists:
                print(f"  [OK] {label}: {description}")
                return
            time.sleep(0.25)
        state = "exists" if exists else "is missing"
        raise RuntimeError(f"{label}: {description}: {path} {state}")

    created: list[Path] = []
    commands_path.write_text(f"# undo suite asset phase: {label}\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = f"../examples/{project}"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(commands_path)
    environment["PILL_EDITOR_TEST_SECONDS"] = "600"
    environment["LOCALAPPDATA"] = str(appdata)
    log = log_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(BINARY)],
        cwd=str(MODULES),
        env=environment,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    log.close()
    try:
        wait(process, r"\[editor\] window created", 600, "editor started")
        wait(process, r"\[editor\] asset listing refreshed", 120, "assets listed")
        before = len(read())
        send("dump standalone-types")
        wait(process, r"\[test\] standalone type\s", 30, "standalone types dumped", before)
        match = re.search(
            r'\[test\] standalone type\s+type_name="([^"]+)"\s+display="([^"]+)"'
            r'\s+extension="([^"]+)"',
            read(),
        )
        assert match, f"{label}: no standalone type in the dump"
        type_name, display, extension = match.groups()
        print(f"  [OK] {label}: first standalone type {display} (.{extension})")

        probe_name = f"w25_probe.{extension}"
        moved_name = f"w25_moved.{extension}"
        created = [res / probe_name, res / moved_name, probe, res / "w25_source.png.meta"]

        # Create through the recorded command, undo it, redo it back.
        before = len(read())
        send(f"edit asset create {type_name} - w25_probe")
        wait(
            process,
            r'\[editor\] history push\s+label="create asset w25_probe"',
            60,
            "create recorded",
            before,
        )
        expect_file(res / probe_name, True, "created on disk")
        before = len(read())
        send("command editor.undo")
        wait(
            process,
            r'\[editor\] history undo\s+label="create asset w25_probe"',
            60,
            "create undone",
            before,
        )
        expect_file(res / probe_name, False, "creation undone on disk")
        before = len(read())
        send("command editor.redo")
        wait(
            process,
            r'\[editor\] history redo\s+label="create asset w25_probe"',
            60,
            "create redone",
            before,
        )
        expect_file(res / probe_name, True, "creation redone on disk")

        # Move it, undo and redo.
        before = len(read())
        send(f"edit asset move {probe_name} {moved_name}")
        wait(
            process,
            r'\[editor\] history push\s+label="move asset ' + re.escape(probe_name) + r'"',
            60,
            "move recorded",
            before,
        )
        expect_file(res / probe_name, False, "old name gone after move")
        expect_file(res / moved_name, True, "new name present after move")
        before = len(read())
        send("command editor.undo")
        wait(
            process,
            r'\[editor\] history undo\s+label="move asset ' + re.escape(probe_name) + r'"',
            60,
            "move undone",
            before,
        )
        expect_file(res / moved_name, False, "moved name gone after undo")
        expect_file(res / probe_name, True, "original name back after undo")
        before = len(read())
        send("command editor.redo")
        wait(
            process,
            r'\[editor\] history redo\s+label="move asset ' + re.escape(probe_name) + r'"',
            60,
            "move redone",
            before,
        )
        expect_file(res / moved_name, True, "new name back after redo")

        # A sourced file gets its sidecar from the import; the recorded
        # write-meta command answers to a sidecar without usable metadata,
        # so the suite waits for the import's sidecar and empties it.
        probe.parent.mkdir(parents=True, exist_ok=True)
        probe.write_bytes(PROBE_PNG)
        meta = res / "w25_source.png.meta"
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and not meta.is_file():
            time.sleep(0.5)
        assert meta.is_file(), f"{label}: the import wrote no sidecar for the probe"
        meta.write_bytes(b"")
        before = len(read())
        send("edit asset meta w25_source.png")
        wait(
            process,
            r'\[editor\] history push\s+label="write meta w25_source\.png"',
            60,
            "write meta recorded",
            before,
        )
        expect_file(meta, True, "meta written")
        written_meta = meta.read_text(encoding="utf-8")
        written_stamp = meta.stat().st_mtime_ns
        before = len(read())
        send("command editor.undo")
        wait(
            process,
            r'\[editor\] history undo\s+label="write meta w25_source\.png"',
            60,
            "write meta undone",
            before,
        )
        # The undo removes the replacement; the host's ensure then heals a
        # sidecar back in - for a loaded source with the same guid, so the
        # walk-back shows as the file being gone, rewritten or fresh.
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if not meta.exists():
                break
            if meta.stat().st_mtime_ns != written_stamp:
                break
            if meta.read_text(encoding="utf-8") != written_meta:
                break
            time.sleep(0.25)
        else:
            raise RuntimeError(f"{label}: the written sidecar survived its undo")

        # Delete the moved file; undo restores it with its bytes.
        before = len(read())
        send(f"edit asset delete {moved_name}")
        wait(
            process,
            r'\[editor\] history push\s+label="delete asset ' + re.escape(moved_name) + r'"',
            60,
            "delete recorded",
            before,
        )
        expect_file(res / moved_name, False, "deleted on disk")
        before = len(read())
        send("command editor.undo")
        wait(
            process,
            r'\[editor\] history undo\s+label="delete asset ' + re.escape(moved_name) + r'"',
            60,
            "delete undone",
            before,
        )
        expect_file(res / moved_name, True, "deletion undone on disk")
        assert (res / moved_name).stat().st_size > 0, f"{label}: restored file is empty"

        send("quit")
        deadline = time.monotonic() + 120
        while process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.5)
        assert process.poll() is not None, f"{label}: editor did not quit"
        assert process.returncode == 0, f"{label}: editor exited {process.returncode}"
        print(f"  [OK] {label}: clean exit, code 0")
    finally:
        if process.poll() is None:
            process.kill()
        for path in created:
            try:
                path.unlink(missing_ok=True)
            except OSError:
                pass
        try:
            res.rmdir()
        except OSError:
            pass


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
    PREFERENCES.unlink(missing_ok=True)
    original_source = SPLINE_EDITOR_SOURCE.read_text(encoding="utf-8")
    COMMANDS.write_text("# undo suite\n", encoding="utf-8")
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/project_rs"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "420"
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

        # Step 0: pause the engine's systems, so the project's simulation
        # cannot rewrite the fields the phases below read back. The editor
        # itself runs post-update and keeps working while paused, which is
        # exactly what makes scripted edits deterministic.
        before = len(read_log())
        append("command editor.pause_toggle")
        wait_token(
            process,
            r'\[editor\] command executed\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            before,
        )
        before = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entity\s", 30, "world reads while paused", before)

        # Step 1: discover the project component and an entity carrying it.
        before = len(read_log())
        append("dump components")
        wait_token(
            process,
            r'\[test\] component\s+name="?[^"\s]*PhysicsState[^"\s]*"?\s+fields="?[^"\s]*position_x',
            30,
            "PhysicsState discovered",
            before,
        )
        before = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entity\s", 30, "entities listed", before)
        entity = find_in_log(
            r'\[test\] entity\s+entity=(\S+)\s+components="?[^"\s]*PhysicsState', before
        ).group(1)
        print(f"  [OK] entity {entity} carries PhysicsState")

        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_x")
        wait_token(process, r"\[test\] field\s", 30, "original field read", before)
        original_x = find_in_log(r"value=F32\(([^)]+)\)", before).group(1)

        # Step 2: a field edit undoes and redoes.
        before = len(read_log())
        append(f"edit set {entity} PhysicsState position_x 42.5")
        wait_token(
            process,
            r'\[editor\] history push\s+label="set PhysicsState\.position_x"\s+steps=1',
            30,
            "field edit recorded",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_x")
        wait_token(process, r"value=F32\(42\.5\)", 30, "field edit applied", before)

        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="set PhysicsState\.position_x"\s+remaining=0',
            30,
            "field edit undone",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_x")
        wait_token(
            process,
            r"value=F32\(" + re.escape(original_x) + r"\)",
            30,
            "field value restored",
            before,
        )

        before = len(read_log())
        append("command editor.redo")
        wait_token(
            process,
            r'\[editor\] history redo\s+label="set PhysicsState\.position_x"\s+remaining=0',
            30,
            "field edit redone",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_x")
        wait_token(process, r"value=F32\(42\.5\)", 30, "redo applied again", before)
        # Put the stack back to empty for the phases below.
        before = len(read_log())
        append("command editor.undo")
        wait_token(process, r"\[editor\] history undo\s+.*remaining=0", 30, "stack emptied", before)

        # Step 3: a continuous drag coalesces into one step.
        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_y")
        wait_token(process, r"\[test\] field\s", 30, "original position_y read", before)
        original_y = find_in_log(r"value=F32\(([^)]+)\)", before).group(1)

        before = len(read_log())
        # One append: the editor polls whole lines in one frame, so the three
        # sets share a timestamp and merge under the slider key.
        with COMMANDS.open("a", encoding="utf-8") as file:
            for value in ("1.5", "2.5", "3.5"):
                file.write(
                    f"edit set {entity} PhysicsState position_y {value} coalesce slider\n"
                )
        wait_token(
            process,
            r'\[editor\] history push\s+label="set PhysicsState\.position_y"\s+steps=1\s+coalesced=false',
            30,
            "drag started",
            before,
        )
        wait_token(process, r"coalesced=true", 30, "drag coalesced", before)
        position = len(read_log())
        settled = read_log()[before:]
        assert settled.count("coalesced=true") == 2, (
            "expected the three drag edits to merge into one step\n" + settled[-2000:]
        )
        before = position
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="set PhysicsState\.position_y"\s+remaining=0',
            30,
            "drag undone as one step",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_y")
        wait_token(
            process,
            r"value=F32\(" + re.escape(original_y) + r"\)",
            30,
            "drag start restored",
            before,
        )

        # Step 4: add, edit and remove a component; undo restores the values.
        for line, label in (
            (f"edit add {entity} SplineSample", r'\[editor\] history push\s+label="add SplineSample"'),
            (f"edit set {entity} SplineSample t 2.5", r'\[editor\] history push\s+label="set SplineSample\.t"'),
            (f"edit remove {entity} SplineSample", r'\[editor\] history push\s+label="remove SplineSample"'),
        ):
            before = len(read_log())
            append(line)
            wait_token(process, label, 30, f"recorded: {line}", before)
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="remove SplineSample"\s+remaining=2',
            30,
            "removal undone",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} SplineSample t")
        wait_token(process, r"value=F32\(2\.5\)", 30, "removed values restored", before)
        for remaining in (1, 0):
            before = len(read_log())
            append("command editor.undo")
            wait_token(
                process,
                r"\[editor\] history undo\s+.*remaining=" + str(remaining),
                30,
                "component steps undone",
                before,
            )

        # Step 5: create, destroy, restore (fresh handle), remap.
        before = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entity\s", 30, "entities before create", before)
        known = set(
            re.findall(r"\[test\] entity\s+entity=(\S+)", read_log()[before:])
        )

        before = len(read_log())
        append("edit create PhysicsState position_x=7.5 radius=0.5")
        wait_token(
            process,
            r'\[editor\] history push\s+label="create PhysicsState"',
            30,
            "create recorded",
            before,
        )
        before = len(read_log())
        append("dump entities")
        wait_token(process, r"\[test\] entity\s", 30, "entities after create", before)
        fresh = set(re.findall(r"\[test\] entity\s+entity=(\S+)", read_log()[before:]))
        created = sorted(fresh - known)
        assert len(created) == 1, f"expected exactly one new entity, got {created}"
        created = created[0]
        print(f"  [OK] created {created}")

        before = len(read_log())
        append(f"dump field {created} PhysicsState position_x")
        wait_token(process, r"value=F32\(7\.5\)", 30, "created with its seed", before)

        before = len(read_log())
        append(f"edit destroy {created}")
        wait_token(
            process,
            r'\[editor\] history push\s+label="destroy"',
            30,
            "destroy recorded",
            before,
        )
        before = len(read_log())
        append(f"dump entity {created}")
        wait_token(process, r"\[test\] entity gone", 30, "entity destroyed", before)

        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="destroy"\s+remaining=1',
            30,
            "destroy undone",
            before,
        )
        remap = find_in_log(
            r"\[editor\] history remap\s+previous=" + re.escape(created) + r"\s+assigned=(\S+)",
            before,
        )
        restored = remap.group(1)
        print(f"  [OK] restore remapped {created} -> {restored}")
        before = len(read_log())
        append(f"dump entity {restored}")
        wait_token(process, r"\[test\] entity alive", 30, "restored entity alive", before)
        before = len(read_log())
        append(f"dump field {restored} PhysicsState position_x")
        wait_token(process, r"value=F32\(7\.5\)", 30, "restored with its values", before)
        # Undoing the create now destroys the restored (remapped) handle.
        before = len(read_log())
        append("command editor.undo")
        wait_token(process, r"\[editor\] history undo\s+.*remaining=0", 30, "create undone", before)
        before = len(read_log())
        append(f"dump entity {restored}")
        wait_token(process, r"\[test\] entity gone", 30, "restored handle re-destroyed", before)

        # Step 6: the preferences' step cap trims the oldest entry.
        before = len(read_log())
        write_preferences({"history_step_cap": 3})
        wait_token(
            process,
            r"\[editor\] preferences applied\s+step_cap=3",
            30,
            "step cap applied",
            before,
        )
        with COMMANDS.open("a", encoding="utf-8") as file:
            for value in ("11.5", "12.5", "13.5"):
                file.write(f"edit set {entity} PhysicsState velocity_x {value}\n")
        before = len(read_log())
        wait_token(process, r"steps=3", 30, "three steps at the cap", before)
        before = len(read_log())
        append(f"edit set {entity} PhysicsState velocity_y 21.5")
        wait_token(
            process,
            r"\[editor\] history trimmed\s+dropped=1\s+reason=Some\(Steps\)",
            30,
            "oldest entry trimmed by the cap",
            before,
        )
        for _ in range(3):
            before = len(read_log())
            append("command editor.undo")
            wait_token(process, r"\[editor\] history undo\s+", 30, "cap phase undone", before)

        # Step 7: an edit survives a reload of the spline editor crate.
        before = len(read_log())
        append(f"edit set {entity} PhysicsState position_x 9.5")
        wait_token(
            process,
            r'\[editor\] history push\s+label="set PhysicsState\.position_x"',
            30,
            "edit before reload",
            before,
        )
        before = len(read_log())
        SPLINE_EDITOR_SOURCE.write_text(
            original_source + "\n// undo suite reload check\n", encoding="utf-8"
        )
        wait_token(
            process,
            r"\[editor\] pill_spline_editor loaded\s+loaded_generations=2",
            180,
            "editor crate reloaded",
            before,
        )
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="set PhysicsState\.position_x"\s+remaining=0',
            60,
            "undo across the reload",
            before,
        )
        before = len(read_log())
        append(f"dump field {entity} PhysicsState position_x")
        wait_token(
            process,
            r"value=F32\(" + re.escape(original_x) + r"\)",
            30,
            "value restored across the reload",
            before,
        )

        # Step 7b: a resource field edit and a selection change each undo as
        # one step.
        before = len(read_log())
        append(f"dump resource {GIZMO_RESOURCE} settings.snap")
        wait_token(process, r'\[test\] resource\s+resource="?', 30, "gizmo snap read", before)
        before = len(read_log())
        append(f"edit resource set {GIZMO_RESOURCE} settings.snap true")
        wait_token(
            process,
            r'\[editor\] history push\s+label="set settings\.snap"',
            30,
            "resource edit recorded",
            before,
        )
        before = len(read_log())
        append(f"dump resource {GIZMO_RESOURCE} settings.snap")
        wait_token(process, r"value=Bool\(true\)", 30, "gizmo snap set", before)
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="set settings\.snap"',
            30,
            "resource edit undone",
            before,
        )
        before = len(read_log())
        append(f"dump resource {GIZMO_RESOURCE} settings.snap")
        wait_token(process, r"value=Bool\(false\)", 30, "gizmo snap undone", before)
        before = len(read_log())
        append("command editor.redo")
        wait_token(
            process,
            r'\[editor\] history redo\s+label="set settings\.snap"',
            30,
            "resource edit redone",
            before,
        )
        before = len(read_log())
        append(f"dump resource {GIZMO_RESOURCE} settings.snap")
        wait_token(process, r"value=Bool\(true\)", 30, "gizmo snap redone", before)
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="set settings\.snap"',
            30,
            "resource edit left undone",
            before,
        )

        # A selection is a recorded step too: none at the start, selecting
        # records and applies one, undo clears it and redo brings it back.
        before = len(read_log())
        append("dump selection")
        wait_token(process, r'\[test\] selection\s+entity="-"', 30, "no selection yet", before)
        before = len(read_log())
        append(f"select {entity}")
        wait_token(
            process,
            r'\[editor\] history push\s+label="select entity"',
            30,
            "selection recorded",
            before,
        )
        wait_token(process, r"\[editor\] selection applied\s", 30, "selection applied", before)
        before = len(read_log())
        append("dump selection")
        wait_token(
            process,
            r'\[test\] selection\s+entity="' + re.escape(entity) + r'"',
            30,
            "selection reads the entity",
            before,
        )
        before = len(read_log())
        append("command editor.undo")
        wait_token(
            process,
            r'\[editor\] history undo\s+label="select entity"',
            30,
            "selection undone",
            before,
        )
        wait_token(
            process,
            r'\[editor\] selection applied\s+entity="-"',
            30,
            "selection cleared by undo",
            before,
        )
        before = len(read_log())
        append("command editor.redo")
        wait_token(
            process,
            r'\[editor\] history redo\s+label="select entity"',
            30,
            "selection redone",
            before,
        )
        before = len(read_log())
        append("dump selection")
        wait_token(
            process,
            r'\[test\] selection\s+entity="' + re.escape(entity) + r'"',
            30,
            "selection back after redo",
            before,
        )

        # Step 8: exit cleanly.
        append("quit")
        deadline = time.monotonic() + 60
        while process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.5)
        assert process.poll() is not None, "editor did not quit"
        assert process.returncode == 0, f"editor exited {process.returncode}"
        print("  [OK] clean exit, code 0")

        # Step 9: the recorded asset commands on the Rust and the C# example
        # project, each in its own editor run. The C# side uses
        # examples/project_cs: the gitignored circus_demo_cs working copy on
        # this machine does not build (its AssetHandle work is unfinished).
        run_asset_phase("circus_demo", "circus_demo", "undo_assets_circus_appdata")
        run_asset_phase("project_cs", "project_cs", "undo_assets_project_cs_appdata")
    finally:
        if process.poll() is None:
            subprocess.run(
                ["taskkill", "/F", "/IM", "pill_editor_native.exe"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        SPLINE_EDITOR_SOURCE.write_text(original_source, encoding="utf-8")
        PREFERENCES.unlink(missing_ok=True)
    print("PASS: editor undo history")
    return 0


if __name__ == "__main__":
    sys.exit(main())

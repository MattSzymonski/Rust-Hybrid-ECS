"""End-to-end acceptance for the editor's undo/redo history (W-0004 Stage 15).

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
    6. an edit survives a reload of the spline editor crate and still undoes.

  Everything is asserted on the editor's log tokens; the edited crate source
  is restored in a `finally`. The suite runs under its own LOCALAPPDATA
  sandbox, so the developer's real editor state is untouched.

USAGE
  python devops/tests/test_editor_undo.py

EXAMPLE USAGE
  python devops/tests/test_editor_undo.py

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
LOG = VALIDATION / "editor-undo.log"
COMMANDS = VALIDATION / "commands-stage15.txt"
APPDATA = VALIDATION / "stage15_appdata"
PREFERENCES = APPDATA / "RustHybridEcs" / "preferences.json"
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
    PREFERENCES.unlink(missing_ok=True)
    original_source = SPLINE_EDITOR_SOURCE.read_text(encoding="utf-8")
    COMMANDS.write_text("# stage 15 undo suite\n", encoding="utf-8")
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
            original_source + "\n// stage 15 suite reload check\n", encoding="utf-8"
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

        # Step 8: exit cleanly.
        append("quit")
        deadline = time.monotonic() + 60
        while process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.5)
        assert process.poll() is not None, "editor did not quit"
        assert process.returncode == 0, f"editor exited {process.returncode}"
        print("  [OK] clean exit, code 0")
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

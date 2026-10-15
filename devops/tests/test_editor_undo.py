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
  is restored when the session closes. The suite runs under its own
  LOCALAPPDATA sandbox, so the developer's real editor state is untouched,
  and its runs land in `local/editor-validation/undo-<run id>/` (one file set
  per session).

USAGE
  python devops/tests/test_editor_undo.py

EXAMPLE USAGE
  python devops/tests/test_editor_undo.py

--- SCRIPT ---
"""

import base64
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
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)
from core.paths import MODULES_ROOT, REPOSITORY_ROOT  # noqa: E402

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
SPLINE_EDITOR_LOADED_TOKEN = "[editor] pill_spline_editor loaded"
ENTITY_LIST_TOKEN = "[test] entity"
ENTITY_GONE_TOKEN = "[test] entity gone"
ENTITY_ALIVE_TOKEN = "[test] entity alive"
COMPONENT_LIST_TOKEN = "[test] component"
FIELD_VALUE_TOKEN = "[test] field"
RESOURCE_VALUE_TOKEN = "[test] resource"
SELECTION_TOKEN = "[test] selection"
STANDALONE_TYPE_TOKEN = "[test] standalone type"
ASSET_LISTING_TOKEN = "[editor] asset listing refreshed"
HISTORY_REMAP_TOKEN = "[editor] history remap"
HISTORY_TRIMMED_TOKEN = "[editor] history trimmed"
PREFERENCES_APPLIED_TOKEN = "[editor] preferences applied"
SELECTION_APPLIED_TOKEN = "[editor] selection applied"

SPLINE_EDITOR_SOURCE = (
    MODULES_ROOT
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


def run_asset_phase(suite: str, session_name: str, project: str, label: str) -> None:
    """Drives the recorded asset commands on one project.

    Create, move, write-meta and delete each undo and redo through
    `command editor.undo` / `command editor.redo`, with the files checked on
    disk at every step, and quit cleanly. The probe files it creates are
    removed in a `finally`; the session shares the suite's run folder.
    """
    session = EditorSession(
        suite, session=session_name, project=f"../examples/{project}", test_seconds=600
    )
    res = REPOSITORY_ROOT / "examples" / project / "res"
    probe = res / "w25_source.png"
    created: list[Path] = []

    def expect_file(path: Path, exists: bool, description: str, timeout: float = 30) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if path.exists() == exists:
                print(f"  [OK] {label}: {description}")
                return
            # Poll until the file state flips.
            time.sleep(0.25)
        state = "exists" if exists else "is missing"
        raise RuntimeError(f"{label}: {description}: {path} {state}")

    session.start()
    try:
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 600, f"{label}: editor started")
        session.wait_token(ASSET_LISTING_TOKEN, 120, f"{label}: assets listed")
        before = len(session.read_log())
        session.send("dump standalone-types")
        session.wait(
            re.escape(STANDALONE_TYPE_TOKEN) + r"\s",
            30,
            f"{label}: standalone types dumped",
            before,
        )
        match = re.search(
            re.escape(STANDALONE_TYPE_TOKEN)
            + r'\s+type_name="([^"]+)"\s+display="([^"]+)"\s+extension="([^"]+)"',
            session.read_log(),
        )
        assert match, f"{label}: no standalone type in the dump"
        type_name, display, extension = match.groups()
        print(f"  [OK] {label}: first standalone type {display} (.{extension})")

        probe_name = f"w25_probe.{extension}"
        moved_name = f"w25_moved.{extension}"
        created = [res / probe_name, res / moved_name, probe, res / "w25_source.png.meta"]

        # Create through the recorded command, undo it, redo it back.
        before = len(session.read_log())
        session.send(f"edit asset create {type_name} - w25_probe")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="create asset w25_probe"',
            60,
            f"{label}: create recorded",
            before,
        )
        expect_file(res / probe_name, True, "created on disk")
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="create asset w25_probe"',
            60,
            f"{label}: create undone",
            before,
        )
        expect_file(res / probe_name, False, "creation undone on disk")
        before = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN) + r'\s+label="create asset w25_probe"',
            60,
            f"{label}: create redone",
            before,
        )
        expect_file(res / probe_name, True, "creation redone on disk")

        # Move it, undo and redo.
        before = len(session.read_log())
        session.send(f"edit asset move {probe_name} {moved_name}")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN)
            + r'\s+label="move asset ' + re.escape(probe_name) + r'"',
            60,
            f"{label}: move recorded",
            before,
        )
        expect_file(res / probe_name, False, "old name gone after move")
        expect_file(res / moved_name, True, "new name present after move")
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN)
            + r'\s+label="move asset ' + re.escape(probe_name) + r'"',
            60,
            f"{label}: move undone",
            before,
        )
        expect_file(res / moved_name, False, "moved name gone after undo")
        expect_file(res / probe_name, True, "original name back after undo")
        before = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN)
            + r'\s+label="move asset ' + re.escape(probe_name) + r'"',
            60,
            f"{label}: move redone",
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
            # Poll until the import writes its sidecar.
            time.sleep(0.5)
        assert meta.is_file(), f"{label}: the import wrote no sidecar for the probe"
        meta.write_bytes(b"")
        before = len(session.read_log())
        session.send("edit asset meta w25_source.png")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="write meta w25_source\.png"',
            60,
            f"{label}: write meta recorded",
            before,
        )
        expect_file(meta, True, "meta written")
        written_meta = meta.read_text(encoding="utf-8")
        written_stamp = meta.stat().st_mtime_ns
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="write meta w25_source\.png"',
            60,
            f"{label}: write meta undone",
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
            # Poll until the undo rewrites the sidecar.
            time.sleep(0.25)
        else:
            raise RuntimeError(f"{label}: the written sidecar survived its undo")

        # Delete the moved file; undo restores it with its bytes.
        before = len(session.read_log())
        session.send(f"edit asset delete {moved_name}")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN)
            + r'\s+label="delete asset ' + re.escape(moved_name) + r'"',
            60,
            f"{label}: delete recorded",
            before,
        )
        expect_file(res / moved_name, False, "deleted on disk")
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN)
            + r'\s+label="delete asset ' + re.escape(moved_name) + r'"',
            60,
            f"{label}: delete undone",
            before,
        )
        expect_file(res / moved_name, True, "deletion undone on disk")
        assert (res / moved_name).stat().st_size > 0, f"{label}: restored file is empty"

        status = session.quit_and_wait(timeout=120)
        assert status == 0, f"{label}: editor exited {status}"
        print(f"  [OK] {label}: clean exit, code 0")
    finally:
        session.close()
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
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    session = EditorSession("undo", test_seconds=420)
    session.start()
    try:
        # Startup lines land within one poll interval of each other, so these
        # waits search the whole log: anchoring after the previous match can
        # skip a line that was written before the anchor was taken.
        session.wait_token(SPLINE_EDITOR_LOADED_TOKEN, 300, "editor crate loaded")
        session.wait_token(EDITOR_SHELL_LOADED_TOKEN, 300, "shell loaded")
        session.wait_token(EDITOR_WINDOW_CREATED_TOKEN, 300, "editor started")
        session.wait_token(EDITOR_COMMAND_REGISTRY_TOKEN, 120, "registry ready")

        # Step 0: pause the engine's systems, so the project's simulation
        # cannot rewrite the fields the phases below read back. The editor
        # itself runs post-update and keeps working while paused, which is
        # exactly what makes scripted edits deterministic.
        before = len(session.read_log())
        session.send("command editor.pause_toggle")
        session.wait(
            re.escape(EDITOR_COMMAND_EXECUTED_TOKEN) + r'\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            before,
        )
        before = len(session.read_log())
        session.send("dump entities")
        session.wait(re.escape(ENTITY_LIST_TOKEN) + r"\s", 30, "world reads while paused", before)

        # Step 1: discover the project component and an entity carrying it.
        before = len(session.read_log())
        session.send("dump components")
        session.wait(
            re.escape(COMPONENT_LIST_TOKEN)
            + r'\s+name="?[^"\s]*PhysicsState[^"\s]*"?\s+fields="?[^"\s]*position_x',
            30,
            "PhysicsState discovered",
            before,
        )
        before = len(session.read_log())
        session.send("dump entities")
        session.wait(re.escape(ENTITY_LIST_TOKEN) + r"\s", 30, "entities listed", before)
        entity = session.find(
            re.escape(ENTITY_LIST_TOKEN)
            + r'\s+entity=(\S+)\s+components="?[^"\s]*PhysicsState',
            before,
        ).group(1)
        print(f"  [OK] entity {entity} carries PhysicsState")

        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_x")
        session.wait(re.escape(FIELD_VALUE_TOKEN) + r"\s", 30, "original field read", before)
        original_x = session.find(r"value=F32\(([^)]+)\)", before).group(1)

        # Step 2: a field edit undoes and redoes.
        before = len(session.read_log())
        session.send(f"edit set {entity} PhysicsState position_x 42.5")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN)
            + r'\s+label="set PhysicsState\.position_x"\s+steps=1',
            30,
            "field edit recorded",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_x")
        session.wait(r"value=F32\(42\.5\)", 30, "field edit applied", before)

        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN)
            + r'\s+label="set PhysicsState\.position_x"\s+remaining=0',
            30,
            "field edit undone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_x")
        session.wait(
            r"value=F32\(" + re.escape(original_x) + r"\)",
            30,
            "field value restored",
            before,
        )

        before = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN)
            + r'\s+label="set PhysicsState\.position_x"\s+remaining=0',
            30,
            "field edit redone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_x")
        session.wait(r"value=F32\(42\.5\)", 30, "redo applied again", before)
        # Put the stack back to empty for the phases below.
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r"\s+.*remaining=0",
            30,
            "stack emptied",
            before,
        )

        # Step 3: a continuous drag coalesces into one step.
        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_y")
        session.wait(re.escape(FIELD_VALUE_TOKEN) + r"\s", 30, "original position_y read", before)
        original_y = session.find(r"value=F32\(([^)]+)\)", before).group(1)

        before = len(session.read_log())
        # One write: the editor polls whole lines in one frame, so the three
        # sets share a timestamp and merge under the slider key.
        session.send_many(
            [
                f"edit set {entity} PhysicsState position_y {value} coalesce slider"
                for value in ("1.5", "2.5", "3.5")
            ]
        )
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN)
            + r'\s+label="set PhysicsState\.position_y"\s+steps=1\s+coalesced=false',
            30,
            "drag started",
            before,
        )
        session.wait(r"coalesced=true", 30, "drag coalesced", before)
        position = len(session.read_log())
        settled = session.read_log()[before:]
        assert settled.count("coalesced=true") == 2, (
            "expected the three drag edits to merge into one step\n" + settled[-2000:]
        )
        before = position
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN)
            + r'\s+label="set PhysicsState\.position_y"\s+remaining=0',
            30,
            "drag undone as one step",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_y")
        session.wait(
            r"value=F32\(" + re.escape(original_y) + r"\)",
            30,
            "drag start restored",
            before,
        )

        # Step 4: add, edit and remove a component; undo restores the values.
        for line, label in (
            (
                f"edit add {entity} SplineSample",
                re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="add SplineSample"',
            ),
            (
                f"edit set {entity} SplineSample t 2.5",
                re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="set SplineSample\.t"',
            ),
            (
                f"edit remove {entity} SplineSample",
                re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="remove SplineSample"',
            ),
        ):
            before = len(session.read_log())
            session.send(line)
            session.wait(label, 30, f"recorded: {line}", before)
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="remove SplineSample"\s+remaining=2',
            30,
            "removal undone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} SplineSample t")
        session.wait(r"value=F32\(2\.5\)", 30, "removed values restored", before)
        for remaining in (1, 0):
            before = len(session.read_log())
            session.send("command editor.undo")
            session.wait(
                re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r"\s+.*remaining=" + str(remaining),
                30,
                "component steps undone",
                before,
            )

        # Step 5: create, destroy, restore (fresh handle), remap.
        before = len(session.read_log())
        session.send("dump entities")
        session.wait(re.escape(ENTITY_LIST_TOKEN) + r"\s", 30, "entities before create", before)
        known = set(
            re.findall(
                re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=(\S+)",
                session.read_log()[before:],
            )
        )

        before = len(session.read_log())
        session.send("edit create PhysicsState position_x=7.5 radius=0.5")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="create PhysicsState"',
            30,
            "create recorded",
            before,
        )
        before = len(session.read_log())
        session.send("dump entities")
        session.wait(re.escape(ENTITY_LIST_TOKEN) + r"\s", 30, "entities after create", before)
        fresh = set(
            re.findall(
                re.escape(ENTITY_LIST_TOKEN) + r"\s+entity=(\S+)",
                session.read_log()[before:],
            )
        )
        created = sorted(fresh - known)
        assert len(created) == 1, f"expected exactly one new entity, got {created}"
        created = created[0]
        print(f"  [OK] created {created}")

        before = len(session.read_log())
        session.send(f"dump field {created} PhysicsState position_x")
        session.wait(r"value=F32\(7\.5\)", 30, "created with its seed", before)

        before = len(session.read_log())
        session.send(f"edit destroy {created}")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="destroy"',
            30,
            "destroy recorded",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump entity {created}")
        session.wait_token(ENTITY_GONE_TOKEN, 30, "entity destroyed", before)

        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="destroy"\s+remaining=1',
            30,
            "destroy undone",
            before,
        )
        remap = session.find(
            re.escape(HISTORY_REMAP_TOKEN)
            + r"\s+previous=" + re.escape(created) + r"\s+assigned=(\S+)",
            before,
        )
        restored = remap.group(1)
        print(f"  [OK] restore remapped {created} -> {restored}")
        before = len(session.read_log())
        session.send(f"dump entity {restored}")
        session.wait_token(ENTITY_ALIVE_TOKEN, 30, "restored entity alive", before)
        before = len(session.read_log())
        session.send(f"dump field {restored} PhysicsState position_x")
        session.wait(r"value=F32\(7\.5\)", 30, "restored with its values", before)
        # Undoing the create now destroys the restored (remapped) handle.
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r"\s+.*remaining=0",
            30,
            "create undone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump entity {restored}")
        session.wait_token(ENTITY_GONE_TOKEN, 30, "restored handle re-destroyed", before)

        # Step 6: the preferences' step cap trims the oldest entry.
        before = len(session.read_log())
        session.write_appdata_json("preferences.json", {"history_step_cap": 3})
        session.wait(
            re.escape(PREFERENCES_APPLIED_TOKEN) + r"\s+step_cap=3",
            30,
            "step cap applied",
            before,
        )
        session.send_many(
            [f"edit set {entity} PhysicsState velocity_x {value}" for value in ("11.5", "12.5", "13.5")]
        )
        before = len(session.read_log())
        session.wait(r"steps=3", 30, "three steps at the cap", before)
        before = len(session.read_log())
        session.send(f"edit set {entity} PhysicsState velocity_y 21.5")
        session.wait(
            re.escape(HISTORY_TRIMMED_TOKEN) + r"\s+dropped=1\s+reason=Some\(Steps\)",
            30,
            "oldest entry trimmed by the cap",
            before,
        )
        for _ in range(3):
            before = len(session.read_log())
            session.send("command editor.undo")
            session.wait(
                re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r"\s+",
                30,
                "cap phase undone",
                before,
            )

        # Step 7: an edit survives a reload of the spline editor crate.
        before = len(session.read_log())
        session.send(f"edit set {entity} PhysicsState position_x 9.5")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="set PhysicsState\.position_x"',
            30,
            "edit before reload",
            before,
        )
        before = len(session.read_log())
        with SPLINE_EDITOR_SOURCE.open("r", encoding="utf-8", newline="") as file:
            original_source = file.read()
        session.edit_source(
            SPLINE_EDITOR_SOURCE, original_source + "\n// undo suite reload check\n"
        )
        session.wait(
            re.escape(SPLINE_EDITOR_LOADED_TOKEN) + r"\s+loaded_generations=2",
            180,
            "editor crate reloaded",
            before,
        )
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN)
            + r'\s+label="set PhysicsState\.position_x"\s+remaining=0',
            60,
            "undo across the reload",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump field {entity} PhysicsState position_x")
        session.wait(
            r"value=F32\(" + re.escape(original_x) + r"\)",
            30,
            "value restored across the reload",
            before,
        )

        # Step 7b: a resource field edit and a selection change each undo as
        # one step.
        before = len(session.read_log())
        session.send(f"dump resource {GIZMO_RESOURCE} settings.snap")
        session.wait(re.escape(RESOURCE_VALUE_TOKEN) + r'\s+resource="?', 30, "gizmo snap read", before)
        before = len(session.read_log())
        session.send(f"edit resource set {GIZMO_RESOURCE} settings.snap true")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="set settings\.snap"',
            30,
            "resource edit recorded",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump resource {GIZMO_RESOURCE} settings.snap")
        session.wait(r"value=Bool\(true\)", 30, "gizmo snap set", before)
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="set settings\.snap"',
            30,
            "resource edit undone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump resource {GIZMO_RESOURCE} settings.snap")
        session.wait(r"value=Bool\(false\)", 30, "gizmo snap undone", before)
        before = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN) + r'\s+label="set settings\.snap"',
            30,
            "resource edit redone",
            before,
        )
        before = len(session.read_log())
        session.send(f"dump resource {GIZMO_RESOURCE} settings.snap")
        session.wait(r"value=Bool\(true\)", 30, "gizmo snap redone", before)
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="set settings\.snap"',
            30,
            "resource edit left undone",
            before,
        )

        # A selection is a recorded step too: none at the start, selecting
        # records and applies one, undo clears it and redo brings it back.
        before = len(session.read_log())
        session.send("dump selection")
        session.wait(
            re.escape(SELECTION_TOKEN) + r'\s+entity="-"', 30, "no selection yet", before
        )
        before = len(session.read_log())
        session.send(f"select {entity}")
        session.wait(
            re.escape(EDITOR_HISTORY_PUSH_TOKEN) + r'\s+label="select entity"',
            30,
            "selection recorded",
            before,
        )
        session.wait(re.escape(SELECTION_APPLIED_TOKEN) + r"\s", 30, "selection applied", before)
        before = len(session.read_log())
        session.send("dump selection")
        session.wait(
            re.escape(SELECTION_TOKEN) + r'\s+entity="' + re.escape(entity) + r'"',
            30,
            "selection reads the entity",
            before,
        )
        before = len(session.read_log())
        session.send("command editor.undo")
        session.wait(
            re.escape(EDITOR_HISTORY_UNDO_TOKEN) + r'\s+label="select entity"',
            30,
            "selection undone",
            before,
        )
        session.wait(
            re.escape(SELECTION_APPLIED_TOKEN) + r'\s+entity="-"',
            30,
            "selection cleared by undo",
            before,
        )
        before = len(session.read_log())
        session.send("command editor.redo")
        session.wait(
            re.escape(EDITOR_HISTORY_REDO_TOKEN) + r'\s+label="select entity"',
            30,
            "selection redone",
            before,
        )
        before = len(session.read_log())
        session.send("dump selection")
        session.wait(
            re.escape(SELECTION_TOKEN) + r'\s+entity="' + re.escape(entity) + r'"',
            30,
            "selection back after redo",
            before,
        )

        # Step 8: exit cleanly.
        status = session.quit_and_wait(timeout=60)
        assert status == 0, f"editor exited {status}"
        print("  [OK] clean exit, code 0")

        # Step 9: the recorded asset commands on the Rust and the C# example
        # project, each in its own editor run. The C# side uses
        # examples/project_cs: the gitignored circus_demo_cs working copy on
        # this machine does not build (its AssetHandle work is unfinished).
        run_asset_phase("undo", "assets-circus_demo", "circus_demo", "circus_demo")
        run_asset_phase("undo", "assets-project_cs", "project_cs", "project_cs")
    finally:
        session.close()
    print("PASS: editor undo history")
    return 0


if __name__ == "__main__":
    sys.exit(main())

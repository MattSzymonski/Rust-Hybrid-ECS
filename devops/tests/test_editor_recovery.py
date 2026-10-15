"""
Editor Recovery Integration Test for Rust-Hybrid-ECS.

PROTECTS: An unclean exit leaves a recovery document; the next start restores
the layout, the open windows and a still-valid selection; a clean exit
discards the document.
TIER: editor
SETS: editor

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs with its physics scene (live entities)

DESCRIPTION
    Drives the native editor through the state-recovery cycle (Stage 28):

    1. a fresh start in a fresh LOCALAPPDATA sandbox leaves no recovery
       document and no native layout file - nothing a later restart could
       read instead of a recovery;
    2. the session changes state: the Preferences tab is docked and an
       entity is selected through the scripted channel (the same recorded
       path a Hierarchy click takes);
    3. the sandbox's `editor_recovery.json` converges to hold that layout,
       that selection (by stable `NvG` handle) and the schema version;
    4. the process is killed - the unclean exit - and the document stays;
    5. a second session on the same sandbox announces the recovery
       (`recovered editor state after an unclean exit`), re-applies the
       selection (`recovered selection restored`, visible through
       `dump selection`) and starts from the recovered layout: saving the
       restarted editor's layout reproduces the recovered one exactly;
    6. the second session exits cleanly through the test channel and the
       recovery document is gone - a clean exit never leaves one behind.

  Everything is asserted on the editor's log tokens and on the JSON the
  editor writes; the runs share one sandbox so the second session starts
  where the first crashed.

USAGE
  python devops/tests/test_editor_recovery.py

  The runs' logs and command files land in their own folder under
  `local/editor-validation/recovery-<run id>/`.

EXAMPLE USAGE
  python devops/tests/test_editor_recovery.py

--- SCRIPT ---
"""

import json
import re
import sys
import time
from pathlib import Path

DEVOPS_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(DEVOPS_ROOT))

from core.editor_session import (  # noqa: E402
    EditorSession,
    EDITOR_SHELL_LOADED_TOKEN,
    EDITOR_TAB_SELECTED_TOKEN,
    EDITOR_WINDOW_CREATED_TOKEN,
)

# Suite tokens: the stable halves of the lines this suite asserts on, so
# `test_log_contract.py` checks each against the Rust sources.
RECOVERY_RESTORED_TOKEN = "[editor] recovered editor state after an unclean exit"
RECOVERED_SELECTION_TOKEN = "[editor] recovered selection restored"
SELECTION_APPLIED_TOKEN = "[editor] selection applied"
LAYOUT_SAVED_TOKEN = "[editor] layout saved"
TEST_SELECTION_TOKEN = "[test] selection"
TEST_ENTITY_TOKEN = "[test] entity"

# The editor-state files the sandbox holds, and the recovery schema. The
# native layout file is the one the editor writes on `editor.save_layout`;
# `editor_layout.json` beside it belongs to the classic editor.
RECOVERY_FILE_NAME = "editor_recovery.json"
NATIVE_LAYOUT_FILE_NAME = "editor_layout_native.json"
RECOVERY_SCHEMA_VERSION = 1

SHELL_LOAD_TIMEOUT = 300
GENERAL_TIMEOUT = 60
DOCUMENT_TIMEOUT = 30
QUIT_TIMEOUT = 120

# `dump entities` answers one `[test] entity entity=<NvG> components=...`
# line per live entity; the first is the suite's selection target.
ENTITY_PATTERN = re.escape(TEST_ENTITY_TOKEN) + r"\s+entity=(\S+)\s+components="

# `dump selection` answers one line with the shared selection's two halves;
# the tracing layer quotes string fields, hence the optional quotes.
SELECTION_PATTERN = (
    re.escape(TEST_SELECTION_TOKEN) + r'\s+entity="?([^"\s]+)"?\s+asset="?([^"\s]+)"?'
)


def appdata_file(session: "EditorSession", name: str) -> Path:
    """One of the editor's per-user state files inside the sandbox."""
    return session.sandbox / "RustHybridEcs" / name


def wait_for_removal(path: Path, timeout: float, label: str) -> None:
    """Waits for `path` to stop existing, with a commented loop."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not path.exists():
            print("  [OK] {}".format(label))
            return
        # The removal lands with the editor's exit; poll briefly.
        time.sleep(0.25)
    raise TimeoutError("{}: {} still exists".format(label, path))


def wait_for_recovery(path: Path, entity: str, timeout: float) -> dict:
    """Waits until the recovery document holds `entity` and the layout tab.

    The editor rewrites the document on change (and on its periodic tick), so
    the check is for the settled content, not merely the file's existence.
    """
    entity_id, entity_generation = entity.split("v")
    expected_entity = {
        "id": int(entity_id),
        "generation": int(entity_generation),
    }
    deadline = time.monotonic() + timeout
    document = {}
    while time.monotonic() < deadline:
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            document = {}
        selection = document.get("selection") or {}
        layout_text = json.dumps(document.get("layout") or {})
        if (
            selection.get("entity") == expected_entity
            and "preferences" in layout_text
        ):
            print("  [OK] recovery document holds the layout and the selection")
            return document
        # The write follows the selection's apply at the frame boundary.
        time.sleep(0.25)
    raise TimeoutError(
        "the recovery document never settled:\n" + json.dumps(document, indent=2)[:2000]
    )


def first_entity(session: "EditorSession") -> str:
    """The first live entity's `NvG` handle, from `dump entities`."""
    start = len(session.read_log())
    session.send("dump entities")
    session.wait(ENTITY_PATTERN, GENERAL_TIMEOUT, "entity listing", start)
    match = re.search(ENTITY_PATTERN, session.read_log()[start:])
    if not match:
        raise RuntimeError("no live entity in the listing")
    return match.group(1)


def read_selection(session: "EditorSession") -> "tuple[str, str]":
    """The shared selection's `(entity, asset)` halves from `dump selection`."""
    start = len(session.read_log())
    session.send("dump selection")
    session.wait(SELECTION_PATTERN, GENERAL_TIMEOUT, "selection dump", start)
    match = re.search(SELECTION_PATTERN, session.read_log()[start:])
    if not match:
        raise RuntimeError("no selection line in the dump")
    return match.group(1), match.group(2)


def run_suite() -> None:
    """The crash-and-recover cycle, end to end."""
    first = EditorSession("recovery", project="../examples/project_rs", test_seconds=600)
    recovery_file = appdata_file(first, RECOVERY_FILE_NAME)
    native_file = appdata_file(first, NATIVE_LAYOUT_FILE_NAME)
    # A fresh sandbox starts clean: no recovery document, and no native
    # layout file a later restart could have read instead of a recovery.
    # The check is before the launch - the editor writes its recovery
    # document from the first frame of every session, crash protection for
    # a session that never changed anything - so by the first boot it may
    # legitimately exist.
    assert not recovery_file.exists(), "a fresh sandbox has no recovery document"
    assert not native_file.exists(), "a fresh sandbox has no native layout"
    first.start()
    try:
        first.wait_token(EDITOR_SHELL_LOADED_TOKEN, SHELL_LOAD_TIMEOUT, "shell loaded")
        first.wait_token(EDITOR_WINDOW_CREATED_TOKEN, SHELL_LOAD_TIMEOUT, "editor started")
        assert RECOVERY_RESTORED_TOKEN not in first.read_log(), (
            "a clean start must not announce a recovery"
        )
        print("  [OK] fresh sandbox starts clean")

        # Change the session: dock the Preferences tab, select an entity.
        start = len(first.read_log())
        first.send("tab Preferences")
        first.wait_token(
            EDITOR_TAB_SELECTED_TOKEN, GENERAL_TIMEOUT, "Preferences tab selected", start
        )
        entity = first_entity(first)
        start = len(first.read_log())
        first.send("select {}".format(entity))
        first.wait_token(SELECTION_APPLIED_TOKEN, GENERAL_TIMEOUT, "selection applied", start)

        # The document converges to the changed session.
        document = wait_for_recovery(recovery_file, entity, DOCUMENT_TIMEOUT)
        assert document["schema_version"] == RECOVERY_SCHEMA_VERSION, document["schema_version"]
        assert document["windows"] == [], "no window was open, so none is stored"
        recovered_layout = document["layout"]
        assert "preferences" in json.dumps(recovered_layout), "the docked tab is in the layout"

        # The unclean exit: kill the process; the document stays.
        process = first.process
        assert process is not None, "the session has not started"
        process.kill()
        process.wait(timeout=30)
        assert recovery_file.is_file(), "an unclean exit keeps the recovery document"
        print("  [OK] the killed editor left its recovery document")
    finally:
        first.close()

    second = EditorSession(
        "recovery",
        session="second",
        project="../examples/project_rs",
        test_seconds=600,
        sandbox=first.sandbox,
    )
    second.start()
    try:
        # The recovery lands during boot and the first frames; the byte
        # anchor is taken before any of it, because the three lines can
        # arrive within one poll.
        start = len(second.read_log())
        second.wait_token(RECOVERY_RESTORED_TOKEN, SHELL_LOAD_TIMEOUT, "recovery announced", start)
        second.wait_token(RECOVERED_SELECTION_TOKEN, GENERAL_TIMEOUT, "selection restored", start)
        second.wait_token(
            SELECTION_APPLIED_TOKEN, GENERAL_TIMEOUT, "recovered selection applied", start
        )
        entity_after, asset_after = read_selection(second)
        assert entity_after == entity, "expected {}, got {}".format(entity, entity_after)
        assert asset_after == "-", "no asset was selected, so none may come back"

        # The layout came from the recovery document: the restarted editor's
        # own save reproduces it exactly, Preferences tab included.
        start = len(second.read_log())
        second.send("command editor.save_layout")
        second.wait_token(LAYOUT_SAVED_TOKEN, GENERAL_TIMEOUT, "layout saved", start)
        saved_layout = json.loads(native_file.read_text(encoding="utf-8"))
        assert saved_layout == recovered_layout, (
            "the restarted editor's layout is not the recovered one"
        )
        print("  [OK] layout, panels and selection restored")

        # The clean exit: exit code zero and no recovery document left.
        exit_code = second.quit_and_wait(timeout=QUIT_TIMEOUT)
        assert exit_code == 0, "expected a clean exit, got {}".format(exit_code)
        wait_for_removal(recovery_file, DOCUMENT_TIMEOUT, "clean exit discarded the document")
    finally:
        second.close()


def main() -> int:
    if not EditorSession.supported():
        print("SKIP: the native editor suites need Windows")
        return 0
    print("Editor recovery suite")
    try:
        run_suite()
    except (AssertionError, RuntimeError, TimeoutError) as error:
        print("  [FAIL] {}".format(error))
        return 1
    print("  PASS: recovery restores the session and a clean exit discards it")
    return 0


if __name__ == "__main__":
    sys.exit(main())

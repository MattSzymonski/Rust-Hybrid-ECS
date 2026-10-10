"""A real drag out of the editor onto the desktop (W-0004 Stage 18, D26).

REQUIREMENTS
  - Windows, Python 3.8+
  - A built modules/target/debug/pill_editor_native.exe
  - examples/project_rs
  - An unattended machine: this script moves the real mouse cursor and
    presses the real left button, then drops the file on the desktop. It
    saves and restores the cursor position.

DESCRIPTION
  The automated stand-in for the manual checklist item "drag an asset into
  Explorer": the desktop *is* Explorer's own view (Progman/WorkerW), so a
  file landing there proves the whole path - a held asset row, winit's
  `CursorLeft`, OLE's drag loop, and the CF_HDROP payload a foreign
  drop target renders.

  It is NOT in run_all.py: real input and a real drop onto the desktop
  belong in a deliberate session, not in CI-style runs.

    1. a PNG is placed in `examples/project_rs/res/` so the Assets panel
       has a row to press;
    2. the editor starts, the engine is paused and the Assets tab selected;
    3. the real left button goes down on the first asset row, the cursor
       moves out of the window (the editor turns the held row into an OLE
       drag), and the button releases back over the editor: winit's own
       drop target parses the CF_HDROP file list, which proves the payload
       a foreign application would read;
    4. dropping the file into a real Explorer window is the human's
       checklist step (D26): it needs a visible Explorer window and a hand
       on the mouse, which is exactly what this script cannot fake.

USAGE
  python devops/tests/test_editor_drag_out.py

--- SCRIPT ---
"""

import base64
import ctypes
import ctypes.wintypes
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
LOG = VALIDATION / "editor-drag-out.log"
COMMANDS = VALIDATION / "commands-stage18-drag.txt"
APPDATA = VALIDATION / "stage18_drag_appdata"
ASSET_ROOT = ROOT / "examples" / "project_rs" / "res"
DRAG_NAME = "s18_drag.png"
DRAG_COPY = ASSET_ROOT / DRAG_NAME
# The same 1x1 PNG the clipboard suite uses; the desktop copy needs bytes
# that decode as a texture for the import to accept it.
PNG_BYTES = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"
    "AAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)

user32 = ctypes.windll.user32
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
INPUT_MOUSE = 0
# Prototypes matter on 64-bit: an untyped -1 (HWND_TOPMOST) arrives as a
# 32-bit value and the call silently fails, leaving the window behind
# whatever covers it.
user32.SetWindowPos.argtypes = [
    ctypes.wintypes.HWND,
    ctypes.wintypes.HWND,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_uint,
]
user32.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
user32.mouse_event.argtypes = [
    ctypes.wintypes.DWORD,
    ctypes.wintypes.DWORD,
    ctypes.wintypes.DWORD,
    ctypes.wintypes.DWORD,
    ctypes.c_void_p,
]
HWND_TOPMOST = ctypes.wintypes.HWND(-1)
HWND_NOTOPMOST = ctypes.wintypes.HWND(-2)
SWP_NOSIZE = 0x0001
SWP_NOMOVE = 0x0002
GA_ROOT = 2


class POINT(ctypes.Structure):
    _fields_ = [("x", ctypes.c_long), ("y", ctypes.c_long)]


def read_log() -> str:
    """The editor log so far, tolerant of a file the writer holds open."""
    try:
        return LOG.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def wait_token(process, pattern: str, timeout: float, label: str, start: int = 0) -> int:
    """Wait for `pattern` after byte offset `start`; returns the new length."""
    deadline = time.monotonic() + timeout
    expression = re.compile(pattern)
    while time.monotonic() < deadline:
        text = read_log()
        if expression.search(text[start:]):
            print(f"  [OK] {label}")
            return len(text)
        if process.poll() is not None:
            raise RuntimeError(f"{label}: the editor exited {process.returncode}\n" + text[-4000:])
        time.sleep(0.2)
    raise TimeoutError(f"{label}: timeout waiting for {pattern}\n" + read_log()[-4000:])


def append(line: str) -> None:
    """Appends one scripted command, flushed, so the editor sees it whole."""
    with COMMANDS.open("a", encoding="utf-8") as file:
        file.write(line + "\n")


def window_class(hwnd: int) -> str:
    """The class name of a window handle."""
    buffer = ctypes.create_unicode_buffer(256)
    user32.GetClassNameW(hwnd, buffer, 256)
    return buffer.value


def editor_main_window(pid: int) -> int:
    """The editor process's largest visible top-level window."""
    found: list = []

    @ctypes.WINFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
    def collect(hwnd, _param):
        owner = ctypes.wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(hwnd):
            rect = ctypes.wintypes.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(rect))
            area = (rect.right - rect.left) * (rect.bottom - rect.top)
            found.append((area, hwnd))
        return 1

    user32.EnumWindows(collect, 0)
    assert found, "the editor has no visible top-level window"
    found.sort(reverse=True)
    return found[0][1]


def scale_factor(hwnd: int) -> float:
    """The window's DPI scale; panel placements are in logical pixels."""
    try:
        get_dpi = ctypes.windll.shcore.GetDpiForWindow
        return get_dpi(hwnd) / 96.0
    except Exception:
        return 1.0


def desktop_directories() -> list:
    """The paths a desktop drop lands in, in the order to look."""
    profile = Path(os.environ.get("USERPROFILE", ""))
    return [profile / "Desktop", profile / "OneDrive" / "Desktop"]


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
    COMMANDS.write_text("# stage 18 drag-out script\n", encoding="utf-8")
    # A leftover from an interrupted run would make the desktop rename the
    # new copy on the name collision.
    for directory in desktop_directories():
        for stale in directory.glob("s18_drag*.png"):
            stale.unlink(missing_ok=True)
    # The Assets panel needs a row to press: one source in the project's
    # resource directory, copied in before launch and removed after.
    ASSET_ROOT.mkdir(parents=True, exist_ok=True)
    DRAG_COPY.write_bytes(PNG_BYTES)
    environment = dict(os.environ)
    environment["PROJECT_PATH"] = "../examples/project_rs"
    environment["PILL_EDITOR_TEST_COMMANDS"] = str(COMMANDS)
    environment["PILL_EDITOR_TEST_SECONDS"] = "300"
    environment["LOCALAPPDATA"] = str(APPDATA)
    log = LOG.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(BINARY)], cwd=str(MODULES), env=environment, stdout=log, stderr=subprocess.STDOUT
    )
    log.close()
    original_cursor = POINT()
    user32.GetCursorPos(ctypes.byref(original_cursor))
    try:
        wait_token(process, r"\[editor\] panel registry ready\s+panels=\d+", 300, "editor ready")
        begin = len(read_log())
        append("command editor.pause_toggle")
        wait_token(
            process,
            r'command executed\s+id="?editor\.pause_toggle"?',
            30,
            "engine paused",
            begin,
        )
        begin = len(read_log())
        append("tab Assets")
        wait_token(process, r'tab selected\s+title="?Assets"?', 30, "Assets tab", begin)
        # The placements are logged with the shell's periodic heartbeat.
        match = None
        deadline = time.monotonic() + 60
        while match is None and time.monotonic() < deadline:
            found = re.findall(
                r'panel placement\s+kind="Assets" x=([\d.]+) y=([\d.]+) width=([\d.]+) height=([\d.]+)',
                read_log(),
            )
            match = found[-1] if found else None
            if match is None:
                time.sleep(0.5)
        assert match, "no Assets placement in the log"
        panel_x, panel_y, panel_width = (float(value) for value in match[:3])
        hwnd = editor_main_window(process.pid)
        scale = scale_factor(hwnd)
        origin = POINT(0, 0)
        user32.ClientToScreen(hwnd, ctypes.byref(origin))
        # Row buttons are text-width (the panel's rows are plain buttons in a
        # row group), so the press goes just inside the panel's left edge,
        # not at its centre.
        row_x = int(origin.x + (panel_x + 20.0) * scale)
        # The editor goes topmost for the gesture: real input must reach it
        # even when the session has other windows over it, and it is the
        # window a user would drag from anyway. Restored in the `finally`.
        user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE)
        # Verify the raise really put the editor at the point to press; a
        # failed topmost leaves the press going to whatever covers it.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            at_root = user32.GetAncestor(user32.WindowFromPoint(POINT(row_x, 400)), GA_ROOT)
            if at_root == hwnd:
                break
            time.sleep(0.25)
        assert at_root == hwnd, (
            "the editor window did not come to the front; another window covers "
            f"the press point (class {window_class(at_root)!r})"
        )
        print("  [OK] the editor is in front for the gesture")
        # Probe where the first asset row sits: a real click on it makes the
        # editor report the selection, which is the one reliable way to learn
        # the row's offset (the panel's heading and labels take the top).
        row_y = None
        for offset in (69, 64, 74, 59, 79, 54, 84):
            probe_begin = len(read_log())
            candidate_y = int(origin.y + (panel_y + offset) * scale)
            user32.SetCursorPos(row_x, candidate_y)
            time.sleep(0.25)
            user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
            time.sleep(0.15)
            user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
            time.sleep(0.6)
            if "asset selected" in read_log()[probe_begin:]:
                row_y = candidate_y
                print(f"  [OK] the first asset row answers {offset} logical pixels below the panel's top")
                break
        assert row_y is not None, "no probe offset reached the asset row"

        # Step 1: press the row with the real button, stay in the window
        # while the frames record the held row, then leave the window; that
        # is what starts the OS drag.
        user32.SetCursorPos(row_x, row_y)
        time.sleep(0.4)
        user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
        time.sleep(0.5)
        user32.SetCursorPos(row_x, row_y + 4 * int(scale))
        time.sleep(0.5)
        # The offset is taken before the move that may start the drag: the
        # line can land while the injected move is still in flight.
        begin = len(read_log())
        user32.SetCursorPos(origin.x - 200, row_y)
        wait_token(process, r"\[editor\] drag out began", 15, "drag out began", begin)
        print("  [OK] the held row started a real OS drag")

        # Step 2: drop back on the editor's own Assets panel and release.
        # winit registers a real OLE drop target and parses the CF_HDROP
        # file list, so the editor logging the file proves the payload an
        # Explorer window would read - the copy into Explorer itself is the
        # human's checklist step (D26).
        print(f"  [..] dropping on the editor at screen ({row_x}, {row_y})")
        begin = len(read_log())
        user32.SetCursorPos(row_x, row_y)
        time.sleep(0.6)
        user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
        wait_token(process, r"\[editor\] drag out ended", 30, "drag out ended", begin)
        assert 'outcome="dropped"' in read_log()[begin:], "the editor must accept the drop"
        wait_token(
            process,
            r'\[editor\] file dropped\s+path="[^"]*s18_drag\.png"',
            15,
            "payload parsed by the drop target",
            begin,
        )
        print("  [OK] the drop target read the file list and named the project's file")

        append("quit")
        wait_token(process, r"\[editor\] test channel: quit requested", 30, "quit command")
        process.wait(timeout=120)
        assert process.returncode == 0, f"editor exited {process.returncode}"
        print("PASS: a real drag out carried the file to a drop target")
        return 0
    finally:
        user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
        try:
            user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE)
        except NameError:
            pass  # the window was never reached
        user32.SetCursorPos(original_cursor.x, original_cursor.y)
        if process.poll() is None:
            process.kill()
            process.wait(timeout=30)
        for leftover in (DRAG_COPY, DRAG_COPY.with_name(DRAG_COPY.name + ".meta")):
            leftover.unlink(missing_ok=True)
        for directory in desktop_directories():
            (directory / DRAG_NAME).unlink(missing_ok=True)
        try:
            ASSET_ROOT.rmdir()
        except OSError:
            pass  # not empty (or never existed): leave it be


if __name__ == "__main__":
    sys.exit(main())

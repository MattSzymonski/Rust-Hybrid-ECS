"""Windows native presentation, lifecycle and clean-shutdown acceptance check.
Run after building the selected frontend. Only touches the process it starts.
"""
import argparse
import ctypes
from ctypes import wintypes
import os
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--project", default="../examples/project_rs")
    parser.add_argument("--cwd", type=Path)
    parser.add_argument("--timeout", type=int, default=240)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    binary = args.binary.resolve()
    logs = root / "local" / "renderer-validation"
    logs.mkdir(parents=True, exist_ok=True)
    log_path = logs / (binary.stem + "-" + Path(args.project).name + "-smoke.log")
    environment = dict(os.environ, PROJECT_PATH=args.project)
    environment["LOCALAPPDATA"] = str(logs / "appdata")
    Path(environment["LOCALAPPDATA"]).mkdir(parents=True, exist_ok=True)
    environment["PATH"] = str(binary.parent / "deps") + os.pathsep + environment["PATH"]
    user32 = ctypes.windll.user32
    user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
    user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
    user32.IsWindowVisible.argtypes = [wintypes.HWND]
    user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
    user32.ShowWindowAsync.argtypes = [wintypes.HWND, ctypes.c_int]
    user32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int, wintypes.UINT]
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen([str(binary)], cwd=args.cwd or root / "modules", env=environment,
                                   stdout=log, stderr=subprocess.STDOUT, creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                text = log_path.read_text(encoding="utf-8", errors="replace")
                if "[render] First frame: Presented; camera=true" in text:
                    break
                if process.poll() is not None:
                    raise RuntimeError(f"frontend exited {process.returncode} before presenting\n{text[-6000:]}")
                time.sleep(0.5)
            else:
                raise TimeoutError(f"first-frame timeout; see {log_path}")
            windows = []
            callback = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
            @callback
            def collect(window, _):
                pid = wintypes.DWORD()
                user32.GetWindowThreadProcessId(window, ctypes.byref(pid))
                if pid.value == process.pid and user32.IsWindowVisible(window):
                    windows.append(window)
                return True
            user32.EnumWindows(collect, 0)
            if not windows:
                raise RuntimeError("frontend presented without a visible native window")
            def area(window):
                rect = wintypes.RECT()
                user32.GetWindowRect(window, ctypes.byref(rect))
                return (rect.right - rect.left) * (rect.bottom - rect.top)
            window = max(windows, key=area)
            print("native scene presented; checking resize", flush=True)
            user32.SetWindowPos(window, None, 40, 40, 960, 720, 0x4004)
            time.sleep(1)
            user32.ShowWindowAsync(window, 6)
            time.sleep(1)
            user32.ShowWindowAsync(window, 9)
            time.sleep(2)
            print("resize/minimize/restore requested; checking shutdown", flush=True)
            user32.PostMessageW(window, 0x0010, 0, 0)
            result = process.wait(timeout=30)
            if result != 0:
                raise RuntimeError(f"frontend shutdown failed: {result}; see {log_path}")
            print(f"PASS: present, resize, minimize/restore, clean shutdown: {binary.name}; {log_path}")
        finally:
            if process.poll() is None:
                subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], capture_output=True)
                process.wait(timeout=30)


if __name__ == "__main__":
    main()

"""
Shared plumbing for the Rust-Hybrid-ECS hot-reload integration suites.

REQUIREMENTS
  - Python 3.8+

DESCRIPTION
    Single source of truth for the low-level machinery every host-driving
    script needs, under `devops/core/` because both the tests and the
    benchmarks import it:

      * `devops/tests/test_hot_reload_suite.py`     - full suite (sessions A/B)
      * `devops/tests/test_hot_reload_migration.py` - schema-migration suite
      * `devops/tests/test_module_project_auto_reload.py` - cascade
      * `devops/tests/test_csharp_bridge.py`        - C# bridge suite
      * `devops/benchmarks/hot_reload_harness.py`   - reload timing harness
      * `devops/benchmarks/cold_start.py`           - host startup timing

    Shared here: path resolution, the console-color `print` wrapper, the
    rolling output monitor with a dedicated counter-tick tail, atomic source
    editing, host process lifecycle helpers, and the log tokens the suites
    assert on.

    Keeping this in one module means a reworded host log line is fixed in
    exactly one place (audit opportunity 5.14), and the OutputMonitor's
    rolling-buffer / tick-tail behavior cannot drift between suites.

USAGE
  from core.suite_common import *   # paths, tokens, print, OutputMonitor, ...

  with `devops/` on sys.path; see `core.paths.ensure_devops_on_path`.

--- SCRIPT ---
"""

import builtins
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Callable, Dict, List, Optional, Sequence, Tuple

# =============================================================================
# Paths
# =============================================================================

# devops/core/suite_common.py -> devops -> repository root.
WORKSPACE_ROOT = Path(__file__).resolve().parents[2]
MODULES_ROOT = WORKSPACE_ROOT / "modules"
HOST_EXE = MODULES_ROOT / "target" / "debug" / "pill_standalone.exe"

# Known project roots. Each declares its own `project_settings.yaml`, the only
# source of the optional-module list; the project itself is selected by
# `PROJECT_PATH` alone (there is no host-level config file anymore).
NATIVE_PROJECT_ROOT = WORKSPACE_ROOT / "examples" / "project_rs"
CSHARP_PROJECT_ROOT = WORKSPACE_ROOT / "examples" / "project_cs"
FIXTURE_PROJECT_ROOT = WORKSPACE_ROOT / "devops" / "tests" / "project"


def project_settings_yaml(project_root: Path) -> Path:
    """Returns a project's settings file path."""
    return project_root / "project_settings.yaml"

# =============================================================================
# Output tokens shared by the suites
#
# The host's log lines are a test contract: the suites assert on literal
# substrings. Keep every token here so a reword is fixed in one place.
#
# HOW THE ASSERTION WORKS, because the failure message does not say so: the
# harness captures the host's merged stdout/stderr into one string, and each
# token is checked with `token not in output`. Nothing is compared against a
# source file. So a token fails for exactly one reason - the host did not print
# that text - and the fix is either to reword the token or to find out why the
# host went quiet.
#
# THE TRAP. Most tokens below are printed by `pill_host`, which nobody edits
# casually. Two are not: MODULE_REGISTERED_MESSAGE and PROJECT_PROBE_PREFIX
# come from the example project and the example module - the two files the
# whole hot-reload demo invites you to type into. Editing the probe string
# breaks these suites with a "Missing required token" that reads like a reload
# failure and is not: the reload worked, the text just moved.
#
# Prefer matching the part of a line that carries meaning - a count, a computed
# value - over the part that is only there to be edited. Where a token still
# pins incidental text, say why at the assertion that uses it.
# =============================================================================

STARTUP_TOKEN = "Entering project loop"
MODULE_LOADED_TOKEN = "module DLL loaded successfully"
ANALYTICS_REPORT_TOKEN = "BUILD / LINK / HOT-RELOAD ANALYTICS"
FAST_PATH_TOKEN = "up to date, skipping build"
RELOAD_PROJECT_TOKEN = "[analytics] reload project"
RELOAD_MODULE_TOKEN = "[analytics] reload pill_spline"
MODULE_RELOAD_COMPLETE_TOKEN = "optional module hot reload complete"
CASCADE_TOKEN = "queuing a project reload"
COUNTER_TICK_TOKEN = "counter tick"
MIGRATION_START_TOKEN = "[persistence] Selective migration starting"
MIGRATION_FINISHED_TOKEN = "[persistence] Selective migration finished"
ROLLBACK_TOKEN = "rolling back"
PANIC_TOKEN = "panicked at"
ACCESS_VIOLATION_TOKEN = "STATUS_ACCESS_VIOLATION"

# Module register-log fields (tracing renders them after the message, e.g.
# "pill_spline module registered splines=1 existing=0 max_control_points=16").
MODULE_REGISTERED_MESSAGE = "pill_spline module registered"
# The project probe line ("[project] xxsees N spline(s), midpoint (...)").
#
# `xxsees` is not a word: it is `sees` with characters typed into it during a
# live-coding session, and it survives only because these suites pin it. That
# is the trap above in miniature - the prefix carries no meaning. If this breaks
# again, prefer widening it to "[project] " and asserting on the spline count,
# rather than restoring whatever the demo happens to print today.
PROJECT_PROBE_PREFIX = "[project] xxsees"

# =============================================================================
# Timeouts (seconds)
# =============================================================================

STARTUP_TIMEOUT = 90
RELOAD_TIMEOUT = 60
PROBE_TIMEOUT = 25
COUNTER_TICK_TIMEOUT = 15
SETTLE_TIMEOUT = 30
STABILITY_SLEEP = 2
SETTLE_SLEEP = 1
PROCESS_KILL_TIMEOUT = 5
BUILD_TIMEOUT = 180
MAX_BUFFERED_LINES = 8000
# Counter-tick lines are captured in a dedicated small tail so the flood can
# never evict the reload lines the scenarios assert on; 200 ticks is still far
# more than enough for any waiter to observe a fresh one.
MAX_TICK_TAIL = 200

# =============================================================================
# Console colors
# =============================================================================

ANSI_RESET = "\033[0m"
ANSI_BOLD = "\033[1m"
ANSI_DIM = "\033[2m"
ANSI_RED = "\033[31m"
ANSI_GREEN = "\033[32m"
ANSI_YELLOW = "\033[33m"
ANSI_BLUE = "\033[34m"
ANSI_MAGENTA = "\033[35m"
ANSI_CYAN = "\033[36m"


def _enable_windows_ansi() -> bool:
    """Enables ANSI color processing on Windows terminals when possible."""
    if os.name != "nt":
        return True
    try:
        import ctypes

        kernel32 = ctypes.windll.kernel32
        standard_output_handle = -11
        enable_virtual_terminal_processing = 0x0004

        handle = kernel32.GetStdHandle(standard_output_handle)
        if handle == 0:
            return False
        mode = ctypes.c_uint32()
        if kernel32.GetConsoleMode(handle, ctypes.byref(mode)) == 0:
            return False
        new_mode = mode.value | enable_virtual_terminal_processing
        if kernel32.SetConsoleMode(handle, new_mode) == 0:
            return False
        return True
    except Exception:
        return False


def _detect_color_support() -> bool:
    """Returns True when stdout supports ANSI colors and NO_COLOR is not set."""
    if os.getenv("NO_COLOR"):
        return False
    if not sys.stdout.isatty():
        return False
    return _enable_windows_ansi()


USE_COLOR = _detect_color_support()

TAG_COLOR_MAP = {
    "[FAIL]": ANSI_BOLD + ANSI_RED,
    "[OK]": ANSI_BOLD + ANSI_GREEN,
    "[WARN]": ANSI_YELLOW,
    "[TEST]": ANSI_BOLD + ANSI_BLUE,
    "[PASS]": ANSI_BOLD + ANSI_GREEN,
    "[PREP]": ANSI_BOLD + ANSI_CYAN,
    "[CLEANUP]": ANSI_BOLD + ANSI_MAGENTA,
    "[timing]": ANSI_CYAN,
    "[std]": ANSI_DIM + ANSI_CYAN,
}


def _colorize_message(message: str) -> str:
    """Applies color to known status tags in one output line."""
    if not USE_COLOR:
        return message
    colored_message = message
    for tag, color_code in TAG_COLOR_MAP.items():
        colored_message = colored_message.replace(tag, f"{color_code}{tag}{ANSI_RESET}")
    return colored_message


def print(*args, **kwargs) -> None:  # type: ignore[override]
    """Print wrapper that colors status tags while preserving normal behavior."""
    output_file = kwargs.get("file", sys.stdout)
    if output_file is not sys.stdout:
        builtins.print(*args, **kwargs)
        return
    separator = kwargs.get("sep", " ")
    end = kwargs.get("end", "\n")
    flush = kwargs.get("flush", False)
    message = separator.join(str(argument) for argument in args)
    builtins.print(_colorize_message(message), end=end, flush=flush)


# =============================================================================
# Suite timing
# =============================================================================


def run_suite_with_timing(main_fn: Callable[[], Optional[int]]) -> Optional[int]:
    """Run one test suite's `main()` wrapped in a start/end/duration log.

    Prints the script's own name with the wall-clock start time before running
    it, then the finish time and the elapsed duration afterwards, tagging the
    outcome with `[PASSED]`/`[FAILED]` (tokens the usual suite console filters
    match). Every suite in `devops/tests/` routes its `__main__` entry through
    this helper so a plain `python devops/tests/test_*.py` run reports how
    long the suite took.

    A `SystemExit` raised inside `main()` is logged with its code and then
    re-raised so the exit status propagates; any other exception is logged and
    re-raised. Returns `main()`'s value (or `None`) on a normal return so
    callers can `sys.exit(run_suite_with_timing(main))`.
    """
    # The real suite script, not sys.argv[0]: a suite may load other devops
    # modules by exec (test_harness_parsing) and mutate sys.argv for them, so
    # argv[0] does not always name the suite being timed.
    main_module = sys.modules.get("__main__")
    main_file = getattr(main_module, "__file__", None) if main_module else None
    script_name = (
        os.path.basename(main_file)
        if main_file
        else (os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else "<suite>")
    )
    started_wall = time.perf_counter()
    started_local = time.strftime("%Y-%m-%d %H:%M:%S")
    print(f"[timing] {script_name}: started at {started_local}")

    def report_finish(verdict: str, note: str = "") -> None:
        elapsed = time.perf_counter() - started_wall
        finished_local = time.strftime("%Y-%m-%d %H:%M:%S")
        suffix = f" ({note})" if note else ""
        print(
            f"[timing] {script_name}: finished at {finished_local} "
            f"after {elapsed:.1f}s [{verdict}]{suffix}"
        )

    try:
        result = main_fn()
    except SystemExit as exit_error:
        # A suite may signal its result through sys.exit() from inside main;
        # report the code, then propagate so the process exits with it.
        code = exit_error.code
        report_finish("PASSED" if code in (None, 0) else "FAILED")
        raise
    except BaseException:
        report_finish("FAILED", "errored")
        raise
    report_finish("PASSED" if result in (None, 0) else "FAILED")
    return result

# =============================================================================
# Backup / restore
# =============================================================================


class BackupRegistry:
    """Captures original file bytes and restores them all at the end."""

    def __init__(self) -> None:
        self._originals: Dict[Path, bytes] = {}

    def capture(self, path: Path) -> None:
        """Records the original content of a file exactly once."""
        if path not in self._originals and path.exists():
            self._originals[path] = path.read_bytes()

    def restore_all(self) -> None:
        """Writes every captured file back to its original bytes."""
        for path, original_bytes in self._originals.items():
            try:
                path.write_bytes(original_bytes)
                print(f"  [CLEANUP] restored {path.relative_to(WORKSPACE_ROOT)}")
            except OSError as error:
                print(f"  [CLEANUP] failed to restore {path}: {error}")

    def restore_one(self, path: Path) -> None:
        """Restores a single captured file to its original bytes."""
        if path in self._originals:
            path.write_bytes(self._originals[path])


# Suite-wide backup singleton: capture every file a suite may touch at startup
# and restore them all (even on failure) at the end.
BACKUP = BackupRegistry()


# =============================================================================
# Source edit helpers
# =============================================================================


def read_source(path: Path) -> str:
    """Reads a source file as UTF-8 text."""
    return path.read_text(encoding="utf-8")


def atomic_write(path: Path, content: str) -> None:
    """Writes source content atomically via temporary file + rename."""
    if not content.endswith("\n"):
        content += "\n"
    temporary_path = path.with_suffix(path.suffix + ".tmp")
    temporary_path.write_text(content, encoding="utf-8")
    os.replace(str(temporary_path), str(path))


def apply_replacements(path: Path, replacements: Sequence[Tuple[str, str]]) -> bool:
    """Applies ordered replacements against the current source content."""
    content = read_source(path)
    for old_text, new_text in replacements:
        if old_text not in content:
            print(
                f"  [FAIL] Edit pattern not found in {path.relative_to(WORKSPACE_ROOT)}: "
                f"{old_text[:80].strip()!r}...",
            )
            return False
        content = content.replace(old_text, new_text, 1)
    atomic_write(path, content)
    return True

# =============================================================================
# Output monitor
# =============================================================================


class OutputMonitor:
    """Captures merged stdout/stderr lines from the host in a background thread.

    Lines are stored as ``(sequence, line)`` pairs with a monotonically
    increasing sequence number, so ``wait_for`` / ``output_since`` stay correct
    even after the rolling buffer drops old entries.

    The host's counter system floods thousands of identical "counter tick"
    lines per second. Those ticks would roll the buffer (and with it every
    reload line) within a second, so tick lines go into a small dedicated tail
    instead of the main buffer: they stay findable for waiters (they are always
    fresh under the flood) without evicting the lines the scenarios assert on.
    """

    def __init__(self, process: subprocess.Popen) -> None:
        self._process = process
        self._lines: List[Tuple[int, str]] = []
        self._tick_tail: List[Tuple[int, str]] = []
        self._sequence = 0
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None

    def start(self) -> None:
        """Starts asynchronous output capture."""
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Signals the output thread to stop."""
        self._stop.set()

    def _read_loop(self) -> None:
        """Reads process output line by line and keeps the rolling buffers."""
        try:
            for line in iter(self._process.stdout.readline, ""):
                if self._stop.is_set():
                    break
                with self._lock:
                    sequence = self._sequence
                    self._sequence += 1
                    if COUNTER_TICK_TOKEN in line:
                        self._tick_tail.append((sequence, line))
                        if len(self._tick_tail) > MAX_TICK_TAIL:
                            self._tick_tail = self._tick_tail[-MAX_TICK_TAIL:]
                    else:
                        self._lines.append((sequence, line))
                        if len(self._lines) > MAX_BUFFERED_LINES:
                            self._lines = self._lines[-MAX_BUFFERED_LINES:]
                # Forward to the console for visibility, but not the counter
                # tick flood - thousands of identical lines a second drown the
                # log without adding information.
                if COUNTER_TICK_TOKEN not in line:
                    print(f"  [std] {line.rstrip()}")
        except (ValueError, OSError):
            pass

    def _iter_since(self, start_index: int):
        """Yields buffered lines with sequence number >= start_index."""
        for sequence, line in self._lines:
            if sequence >= start_index:
                yield line
        for sequence, line in self._tick_tail:
            if sequence >= start_index:
                yield line

    @property
    def line_count(self) -> int:
        """Returns the sequence number of the next line to arrive."""
        with self._lock:
            return self._sequence

    def output_since(self, start_index: int) -> str:
        """Returns output buffered for sequence numbers >= start_index."""
        with self._lock:
            return "".join(self._iter_since(start_index))

    def process_alive(self) -> bool:
        """Returns True while the process is still running."""
        return self._process.poll() is None

    def wait_for(self, token: str, timeout_seconds: float, start_index: int = 0) -> bool:
        """Waits until a token appears in the output (or timeout / exit)."""
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if self._process.poll() is not None:
                return False
            with self._lock:
                for line in self._iter_since(start_index):
                    if token in line:
                        return True
            time.sleep(0.1)
        return False

    def wait_for_any(
        self,
        tokens: Sequence[str],
        timeout_seconds: float,
        start_index: int = 0,
    ) -> Optional[str]:
        """Waits until any of the given tokens appears; returns it or None."""
        deadline = time.monotonic() + timeout_seconds
        while time.monotonic() < deadline:
            if self._process.poll() is not None:
                return None
            with self._lock:
                for line in self._iter_since(start_index):
                    for token in tokens:
                        if token in line:
                            return token
            time.sleep(0.1)
        return None

# =============================================================================
# Process lifecycle helpers
# =============================================================================


def has_crash_signals(output: str) -> bool:
    """Checks output for known crash signatures."""
    return PANIC_TOKEN in output or ACCESS_VIOLATION_TOKEN in output


def launch_process(
    command: Sequence[str],
    cwd: Path,
    environment: Optional[Dict[str, str]] = None,
) -> Tuple[subprocess.Popen, OutputMonitor]:
    """Launches a process with merged output capture and returns (process, monitor)."""
    process = subprocess.Popen(
        list(command),
        cwd=str(cwd),
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    monitor = OutputMonitor(process)
    monitor.start()
    return process, monitor


def terminate_process(process: subprocess.Popen, monitor: OutputMonitor) -> None:
    """Stops the monitor and terminates the host safely."""
    monitor.stop()
    try:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=PROCESS_KILL_TIMEOUT)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    except (OSError, subprocess.SubprocessError):
        pass


def kill_stale_hosts() -> None:
    """Best-effort cleanup of leftover host processes that lock shared DLLs."""
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/IM", "pill_standalone.exe", "/F"],
            capture_output=True,
        )
    else:
        subprocess.run(["pkill", "-f", "pill_standalone"], capture_output=True)


def write_project_settings(project_root: Path, content: str) -> None:
    """Writes a project's `project_settings.yaml`, the file the host reads."""
    atomic_write(project_settings_yaml(project_root), content)


# Names importable via `from suite_common import *`.
__all__ = [
    "WORKSPACE_ROOT",
    "MODULES_ROOT",
    "HOST_EXE",
    "NATIVE_PROJECT_ROOT",
    "CSHARP_PROJECT_ROOT",
    "FIXTURE_PROJECT_ROOT",
    "project_settings_yaml",
    "write_project_settings",
    "STARTUP_TOKEN",
    "MODULE_LOADED_TOKEN",
    "ANALYTICS_REPORT_TOKEN",
    "FAST_PATH_TOKEN",
    "RELOAD_PROJECT_TOKEN",
    "RELOAD_MODULE_TOKEN",
    "MODULE_RELOAD_COMPLETE_TOKEN",
    "CASCADE_TOKEN",
    "COUNTER_TICK_TOKEN",
    "MIGRATION_START_TOKEN",
    "MIGRATION_FINISHED_TOKEN",
    "ROLLBACK_TOKEN",
    "PANIC_TOKEN",
    "ACCESS_VIOLATION_TOKEN",
    "MODULE_REGISTERED_MESSAGE",
    "PROJECT_PROBE_PREFIX",
    "STARTUP_TIMEOUT",
    "RELOAD_TIMEOUT",
    "PROBE_TIMEOUT",
    "COUNTER_TICK_TIMEOUT",
    "SETTLE_TIMEOUT",
    "STABILITY_SLEEP",
    "SETTLE_SLEEP",
    "PROCESS_KILL_TIMEOUT",
    "BUILD_TIMEOUT",
    "MAX_BUFFERED_LINES",
    "MAX_TICK_TAIL",
    "USE_COLOR",
    "TAG_COLOR_MAP",
    "print",
    "run_suite_with_timing",
    "BackupRegistry",
    "BACKUP",
    "read_source",
    "atomic_write",
    "apply_replacements",
    "OutputMonitor",
    "has_crash_signals",
    "launch_process",
    "terminate_process",
    "kill_stale_hosts",
]

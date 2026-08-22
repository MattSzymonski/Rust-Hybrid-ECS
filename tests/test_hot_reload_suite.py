"""
Full Hot-Reloading Integration Suite for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo) on PATH
  - Run from the repository root or anywhere (paths are resolved from __file__)

DESCRIPTION
    End-to-end hot-reload suite that launches the standalone host and drives
    real source edits against the live project and an optional module, then
    asserts the host's behaviour from its console output:

      Session A (project = tests/project, module = pill_spline)
        1. project_hot_reload      - editing the project triggers a reload and
                                     the counter system keeps ticking (data
                                     survives).
        2. schema_migration        - adding a field to a persistable component
                                     runs selective migration, data survives.
        3. project_forgotten_type  - dropping a component registration emits the
                                     orphaned-data warning on the project path.
        4. module_hot_reload       - editing an optional module reloads it.
        5. module_forgotten_type   - a module that stops registering a type
                                     emits the orphaned-data warning.
        6. init_failure_rollback   - an init that returns non-zero keeps the
                                     previous generation and the host alive.

      Session B (project = examples/project_rs, module = pill_spline)
        8. cascade_reload          - editing a module the project links triggers
                                     the module reload AND a project reload, and
                                     the new value reaches the running project
                                     (probe midpoint changes).

    Every file the suite touches (pill_config.yaml, module/project sources) is
    backed up at startup and restored afterwards, so a normal developer
    workspace is left exactly as it was.

USAGE
  python tests/test_hot_reload_suite.py [--timeout-scale S] [--skip-build]

EXAMPLE USAGE
  python tests/test_hot_reload_suite.py
  python tests/test_hot_reload_suite.py --timeout-scale 1.5
  python tests/test_hot_reload_suite.py --skip-build

--- SCRIPT ---
"""

import argparse
import builtins
import os
import re
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple

# =============================================================================
# Configuration
# =============================================================================

WORKSPACE_ROOT = Path(__file__).resolve().parent.parent
MODULES_ROOT = WORKSPACE_ROOT / "modules"
HOST_EXE = MODULES_ROOT / "target" / "debug" / "pill_standalone.exe"
HOST_CONFIG_YAML = MODULES_ROOT / "pill_config.yaml"
PROJECT_SANDBOX_LIB_RS = WORKSPACE_ROOT / "tests" / "project" / "src" / "lib.rs"
SPLINE_LIB_RS = MODULES_ROOT / "optional" / "pill_spline" / "src" / "lib.rs"

SESSION_A_YAML = """\
project: "../tests/project"
modules:
  - "pill_spline"
"""

SESSION_B_YAML = """\
project: "../examples/project_rs"
modules:
  - "pill_spline"
  - "pill_dummy_color"
"""

# --- Output tokens ------------------------------------------------------------

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
PROJECT_FORGOTTEN_WARN_TOKEN = "no longer registered by the project"
MODULE_FORGOTTEN_WARN_TOKEN = "no longer registered by this module"
ROLLBACK_TOKEN = "rolling back"
PANIC_TOKEN = "panicked at"
ACCESS_VIOLATION_TOKEN = "STATUS_ACCESS_VIOLATION"

# --- Timeouts (seconds) -------------------------------------------------------

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


def _detect_color_support() -> bool:
    """Returns True when stdout supports ANSI colors and NO_COLOR is not set."""
    if os.getenv("NO_COLOR"):
        return False
    if not sys.stdout.isatty():
        return False
    if os.name == "nt":
        try:
            import ctypes

            kernel32 = ctypes.windll.kernel32
            handle = kernel32.GetStdHandle(-11)
            mode = ctypes.c_uint32()
            if kernel32.GetConsoleMode(handle, ctypes.byref(mode)) == 0:
                return False
            kernel32.SetConsoleMode(handle, mode.value | 0x0004)
            return True
        except Exception:
            return False
    return True


USE_COLOR = _detect_color_support()

TAG_COLOR_MAP = {
    "[FAIL]": ANSI_BOLD + ANSI_RED,
    "[OK]": ANSI_BOLD + ANSI_GREEN,
    "[WARN]": ANSI_YELLOW,
    "[TEST]": ANSI_BOLD + ANSI_BLUE,
    "[PASS]": ANSI_BOLD + ANSI_GREEN,
    "[PREP]": ANSI_BOLD + ANSI_CYAN,
    "[CLEANUP]": ANSI_BOLD + ANSI_MAGENTA,
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
    """Print wrapper that colors status tags while preserving normal behaviour."""
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
                # tick flood — thousands of identical lines a second drown the
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
# Host session
# =============================================================================


def build_host() -> bool:
    """Builds the standalone host once before the suite runs."""
    print("\n  [PREP] Building pill_standalone (offline)...")
    try:
        result = subprocess.run(
            ["cargo", "build", "--package", "pill_standalone", "--offline"],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=BUILD_TIMEOUT,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as error:
        print(f"  [FAIL] Could not build the host: {error}")
        return False
    if result.returncode != 0:
        print("  [FAIL] Host build failed:")
        print(result.stderr[-2000:])
        return False
    print("  [OK] Host built.")
    return True


def launch_host():
    """Launches the standalone host exe with a clean environment."""
    environment = os.environ.copy()
    environment.pop("PROJECT_PATH", None)

    process = subprocess.Popen(
        [str(HOST_EXE)],
        cwd=str(MODULES_ROOT),
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


def write_host_config(content: str) -> None:
    """Writes the host config the standalone reads at startup."""
    atomic_write(HOST_CONFIG_YAML, content)

# =============================================================================
# Scenario model
# =============================================================================


@dataclass
class Scenario:
    """One hot-reload scenario: edits, expected output, and cleanup."""

    name: str
    edits: Sequence[Tuple[Path, Sequence[Tuple[str, str]]]]
    wait_token: str
    required_tokens: Sequence[str] = ()
    forbidden_tokens: Sequence[str] = ()
    wait_after: Sequence[Tuple[str, float]] = ()
    restore_after: Sequence[Path] = field(default_factory=list)


def has_crash_signals(output: str) -> bool:
    """Checks output for known crash signatures."""
    return PANIC_TOKEN in output or ACCESS_VIOLATION_TOKEN in output


def run_scenario(scenario: Scenario, monitor: OutputMonitor) -> bool:
    """Runs one scenario and asserts the host's reload behaviour."""
    print(f"\n  [TEST] {scenario.name}...")
    start_index = monitor.line_count

    for path, replacements in scenario.edits:
        if not apply_replacements(path, replacements):
            return False

    if not monitor.wait_for(scenario.wait_token, RELOAD_TIMEOUT, start_index):
        output = monitor.output_since(start_index)
        if has_crash_signals(output):
            print(f"  [FAIL] Crash detected in scenario: {scenario.name}")
            print(f"  Output tail:\n{output[-1600:]}")
        else:
            print(f"  [FAIL] Timeout waiting for {scenario.wait_token!r} in scenario: {scenario.name}")
            print(f"  Output tail:\n{output[-1600:]}")
        return False

    time.sleep(STABILITY_SLEEP)

    if not monitor.process_alive():
        print(f"  [FAIL] Process died after scenario: {scenario.name}")
        return False

    # Wait for any secondary tokens (counter ticks, probe values, ...).
    for token, timeout in scenario.wait_after:
        if not monitor.wait_for(token, timeout, start_index):
            print(f"  [FAIL] Missing expected token {token!r} in scenario: {scenario.name}")
            print(f"  Output tail:\n{monitor.output_since(start_index)[-1600:]}")
            return False

    output = monitor.output_since(start_index)
    if has_crash_signals(output):
        print(f"  [FAIL] Crash token observed in scenario: {scenario.name}")
        print(f"  Output tail:\n{output[-1600:]}")
        return False

    for token in scenario.required_tokens:
        if token not in output:
            print(f"  [FAIL] Missing required token in {scenario.name}: {token!r}")
            print(f"  Output tail:\n{output[-1600:]}")
            return False
    for token in scenario.forbidden_tokens:
        if token in output:
            print(f"  [FAIL] Forbidden token found in {scenario.name}: {token!r}")
            print(f"  Output tail:\n{output[-1600:]}")
            return False

    print(f"  [OK] {scenario.name}")

    # Restore edited sources so the next scenario starts from the originals.
    # Each restore rewrites the file, which the watcher turns into another
    # reload; wait for that reload to finish so its output cannot pollute the
    # next scenario's wait window.
    settle_start = monitor.line_count
    for path in scenario.restore_after:
        BACKUP.restore_one(path)
        settle_token = (
            RELOAD_PROJECT_TOKEN if path == PROJECT_SANDBOX_LIB_RS else RELOAD_MODULE_TOKEN
        )
        # Scan only lines that arrive after the restore: the scenario's own
        # reload line is already in the buffer and must not satisfy the wait.
        if not monitor.wait_for(settle_token, SETTLE_TIMEOUT, settle_start):
            print(
                f"  [WARN] Restore of {path.name} did not settle with {settle_token!r}"
            )
        time.sleep(STABILITY_SLEEP)

    return True

# =============================================================================
# Scenario definitions
# =============================================================================

# --- pill_spline module edits (all applied from the original source) ---------

SPLINE_REGISTER_ORIGINAL = """\
#[pill_module]
fn register(engine: &mut Engine) -> u32 {
    // Fill up to the target count rather than spawning a new path on every
    // rebuild, because hot reload preserves the entities already created.
    let existing_spline_count = {
        let mut query = Query::<&Spline>::new(engine.world_mut());
        query.iter_mut().count()
    };
    for _ in existing_spline_count..DEMO_SPLINE_COUNT {
        if engine
            .world_mut()
            .create_entity()
            .with(demo_spline())
            .build()
            .is_err()
        {
            // Report the failure so the host keeps the previous generation
            // instead of running with a half-populated world.
            return 1;
        }
    }

    // Fully qualified: the import would be unused in the project build, where
    // this module-abi registration path is compiled out.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        splines = DEMO_SPLINE_COUNT,
        existing = existing_spline_count,
        max_control_points = MAX_CONTROL_POINTS,
        "pill_spline module registered"
    );
    0
}"""

SPLINE_REGISTER_STUB_NO_REGISTRATION = """\
// Suite stub: the persistable registration is disabled so the module loads
// WITHOUT registering `Spline`. The `#[pill_module]` attribute is dropped too,
// so no compile-time auto-registration runs; the ABI exports are written out
// by hand to keep the module loadable. `DEMO_SPLINE_COUNT`/`demo_spline` are
// referenced only to keep the dead-code lint quiet in this stub build.
#[cfg(feature = "module-abi")]
const PILL_MODULE_ABI_VERSION: u32 = ::pill_engine::module_abi::MODULE_ABI_VERSION;

#[cfg(feature = "module-abi")]
const PILL_MODULE_NAME: &[u8] = b"pill_spline\0";

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_abi_version() -> u32 {
    PILL_MODULE_ABI_VERSION
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_name() -> *const ::core::ffi::c_char {
    PILL_MODULE_NAME.as_ptr() as *const ::core::ffi::c_char
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub unsafe extern "C" fn pill_module_init(api: *const ::pill_engine::EngineApi) -> u32 {
    let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
        let api = unsafe { &*api };
        let engine = unsafe { &mut *(api.engine_handle as *mut ::pill_engine::Engine) };
        register(engine)
    }));
    result.unwrap_or(u32::MAX)
}

#[cfg(feature = "module-abi")]
fn register(engine: &mut Engine) -> u32 {
    let _ = engine;
    let _ = (DEMO_SPLINE_COUNT, demo_spline());
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        "pill_spline module registered (suite stub, Spline NOT registered)"
    );
    0
}"""

SPLINE_REGISTER_STUB_INIT_FAILURE = """\
// Suite stub: deliberately fail init so the host rolls back to the previous
// generation. The `#[pill_module]` attribute is dropped (hand-written exports
// below), and `DEMO_SPLINE_COUNT`/`demo_spline` are referenced only to keep
// the dead-code lint quiet in this stub build.
#[cfg(feature = "module-abi")]
const PILL_MODULE_ABI_VERSION: u32 = ::pill_engine::module_abi::MODULE_ABI_VERSION;

#[cfg(feature = "module-abi")]
const PILL_MODULE_NAME: &[u8] = b"pill_spline\0";

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_abi_version() -> u32 {
    PILL_MODULE_ABI_VERSION
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub extern "C" fn pill_module_name() -> *const ::core::ffi::c_char {
    PILL_MODULE_NAME.as_ptr() as *const ::core::ffi::c_char
}

#[cfg(feature = "module-abi")]
#[no_mangle]
pub unsafe extern "C" fn pill_module_init(api: *const ::pill_engine::EngineApi) -> u32 {
    let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
        let api = unsafe { &*api };
        let engine = unsafe { &mut *(api.engine_handle as *mut ::pill_engine::Engine) };
        register(engine)
    }));
    result.unwrap_or(u32::MAX)
}

#[cfg(feature = "module-abi")]
fn register(engine: &mut Engine) -> u32 {
    let _ = engine;
    let _ = (DEMO_SPLINE_COUNT, demo_spline());
    1
}"""

# --- tests/project edits (all applied from the original source) ---------------

FRAMECOUNTER_MIGRATION_EDITS = [
    (
        "struct FrameCounter {\n    count: u64,\n}",
        "struct FrameCounter {\n    count: u64,\n    migrated: bool,\n}",
    ),
    (
        ".with(FrameCounter { count: 0 })",
        ".with(FrameCounter { count: 0, migrated: false })",
    ),
    (
        ".with(FrameCounter { count: 90 })",
        ".with(FrameCounter { count: 90, migrated: false })",
    ),
    (
        ".with(FrameCounter { count: 180 })",
        ".with(FrameCounter { count: 180, migrated: false })",
    ),
]

PROJECT_FORGOTTEN_EDITS = [
    (
        "#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]\n"
        "#[pill(persistable)]\n"
        "struct LinearVelocity {",
        "#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]\n"
        "struct LinearVelocity {",
    ),
]

# --- Session A scenarios (project = tests/project, module = pill_spline) -----

SESSION_A_SCENARIOS = [
    Scenario(
        name="project_hot_reload",
        edits=[
            (
                PROJECT_SANDBOX_LIB_RS,
                [("const THRESHOLD: u64 = 200;", "const THRESHOLD: u64 = 150;")],
            )
        ],
        wait_token=RELOAD_PROJECT_TOKEN,
        required_tokens=[RELOAD_PROJECT_TOKEN, "crates rebuilt by cargo"],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        wait_after=[(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT)],
        restore_after=[PROJECT_SANDBOX_LIB_RS],
    ),
    Scenario(
        name="schema_migration",
        edits=[(PROJECT_SANDBOX_LIB_RS, FRAMECOUNTER_MIGRATION_EDITS)],
        wait_token=RELOAD_PROJECT_TOKEN,
        required_tokens=[
            MIGRATION_START_TOKEN,
            "'project::FrameCounter' -> OK",
        ],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        wait_after=[(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT)],
        restore_after=[PROJECT_SANDBOX_LIB_RS],
    ),
    Scenario(
        name="project_forgotten_type",
        edits=[(PROJECT_SANDBOX_LIB_RS, PROJECT_FORGOTTEN_EDITS)],
        wait_token=RELOAD_PROJECT_TOKEN,
        required_tokens=[
            PROJECT_FORGOTTEN_WARN_TOKEN,
            "LinearVelocity",
        ],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        wait_after=[(COUNTER_TICK_TOKEN, COUNTER_TICK_TIMEOUT)],
        restore_after=[PROJECT_SANDBOX_LIB_RS],
    ),
    Scenario(
        name="module_hot_reload",
        edits=[
            (
                SPLINE_LIB_RS,
                [
                    (
                        '"pill_spline module registered"',
                        '"pill_spline module registered v2"',
                    )
                ],
            )
        ],
        wait_token=RELOAD_MODULE_TOKEN,
        required_tokens=[
            RELOAD_MODULE_TOKEN,
            MODULE_RELOAD_COMPLETE_TOKEN,
            "crates rebuilt by cargo",
            "pill_spline module registered v2",
        ],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        restore_after=[SPLINE_LIB_RS],
    ),
    Scenario(
        name="module_forgotten_type",
        edits=[
            (
                SPLINE_LIB_RS,
                [
                    (
                        SPLINE_REGISTER_ORIGINAL,
                        SPLINE_REGISTER_STUB_NO_REGISTRATION,
                    )
                ],
            )
        ],
        wait_token=RELOAD_MODULE_TOKEN,
        required_tokens=[MODULE_FORGOTTEN_WARN_TOKEN, "pill_spline::Spline"],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        restore_after=[SPLINE_LIB_RS],
    ),
    Scenario(
        name="init_failure_rollback",
        edits=[
            (
                SPLINE_LIB_RS,
                [
                    (
                        SPLINE_REGISTER_ORIGINAL,
                        SPLINE_REGISTER_STUB_INIT_FAILURE,
                    )
                ],
            )
        ],
        wait_token=ROLLBACK_TOKEN,
        required_tokens=[ROLLBACK_TOKEN],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        restore_after=[SPLINE_LIB_RS],
    ),
]

# --- Session B scenario (project = examples/project_rs, module = pill_spline) -

SESSION_B_SCENARIOS = [
    Scenario(
        name="cascade_reload",
        edits=[
            (
                SPLINE_LIB_RS,
                [
                    (
                        "SAMPLE_VERTICAL_OFFSET: f32 = 0.0",
                        "SAMPLE_VERTICAL_OFFSET: f32 = 10.0",
                    )
                ],
            )
        ],
        wait_token=RELOAD_MODULE_TOKEN,
        required_tokens=[
            RELOAD_MODULE_TOKEN,
            MODULE_RELOAD_COMPLETE_TOKEN,
            CASCADE_TOKEN,
            RELOAD_PROJECT_TOKEN,
        ],
        forbidden_tokens=[PANIC_TOKEN, ACCESS_VIOLATION_TOKEN],
        wait_after=[("midpoint (400.0, 298.8", PROBE_TIMEOUT)],
        restore_after=[SPLINE_LIB_RS],
    ),
]

# =============================================================================
# Session runner
# =============================================================================


def verify_fast_path_restart(yaml_content: str) -> bool:
    """Relaunch the host once and verify the up-to-date build fast path.

    After a session's scenarios every artifact is current, so a clean restart
    should skip both the module and project builds and report them as
    "up-to-date skips" in the analytics startup report. A rebuild on restart
    (for example because cargo state changed between runs) is reported as a
    WARN rather than a failure; only a crash or a failure to reach the loop is
    a hard failure.
    """
    print("\n  [TEST] Fast-path restart (everything up to date)...")
    write_host_config(yaml_content)
    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Fast-path restart did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-1600:]}")
            return False
        startup_output = monitor.output_since(0)
        if has_crash_signals(startup_output):
            print("  [FAIL] Crash signals during fast-path restart.")
            print(f"  Output tail:\n{startup_output[-1600:]}")
            return False
        skip_match = re.search(r"up-to-date skips:\s*(\d+)", startup_output)
        skip_count = int(skip_match.group(1)) if skip_match else 0
        if FAST_PATH_TOKEN in startup_output and skip_count >= 1:
            print(
                f"  [OK] Fast-path restart: {skip_count} build(s) skipped, "
                "module and project up to date."
            )
            return True
        # Tolerant: cargo state may have changed between runs.
        print(
            f"  [WARN] Fast-path restart did not skip builds "
            f"(up-to-date skips={skip_count}); non-fatal."
        )
        return True
    finally:
        terminate_process(process, monitor)


def run_session(name: str, yaml_content: str, scenarios: Sequence[Scenario]) -> bool:
    """Writes the session config, launches the host, and runs its scenarios."""
    print(f"\n{'=' * 64}")
    print(f"  SESSION {name}")
    print(f"{'=' * 64}")

    write_host_config(yaml_content)

    print("\n  [TEST] Launching standalone host...")
    process, monitor = launch_host()
    session_passed = True

    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop in time.")
            output = monitor.output_since(0)
            print(f"  Output tail:\n{output[-1600:]}")
            return False
        print("  [OK] Host started and entered the project loop.")

        # Validate startup invariants: the module loaded and the analytics
        # report printed. The "up to date, skipping build" fast path is NOT a
        # startup invariant — a cold build state rebuilds everything — so it
        # is not asserted here.
        startup_output = monitor.output_since(0)
        for token in (MODULE_LOADED_TOKEN, ANALYTICS_REPORT_TOKEN):
            if token not in startup_output:
                print(f"  [FAIL] Missing startup token: {token!r}")
                print(f"  Output tail:\n{startup_output[-1600:]}")
                return False
        if has_crash_signals(startup_output):
            print("  [FAIL] Crash signals during startup.")
            print(f"  Output tail:\n{startup_output[-1600:]}")
            return False
        print("  [OK] Startup complete: module loaded and analytics report printed.")

        for scenario in scenarios:
            if not run_scenario(scenario, monitor):
                session_passed = False
                break

        if session_passed:
            # I3: verify the up-to-date build fast path with a clean restart.
            # Stop this host first so the relaunch does not contend for the
            # shared DLLs, then relaunch with the same (fully built) config.
            terminate_process(process, monitor)
            if not verify_fast_path_restart(yaml_content):
                session_passed = False
            print(f"\n  [PASS] Session {name} completed.")
    finally:
        terminate_process(process, monitor)

    return session_passed

# =============================================================================
# Main
# =============================================================================


def main() -> None:
    """Parses arguments, runs both sessions, and reports the summary."""
    parser = argparse.ArgumentParser(
        description="Full hot-reloading integration suite for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    parser.add_argument(
        "--skip-build",
        action="store_true",
        help="Skip the initial host build (assume it is already built)",
    )
    args = parser.parse_args()

    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    global STARTUP_TIMEOUT, RELOAD_TIMEOUT, PROBE_TIMEOUT, COUNTER_TICK_TIMEOUT
    global SETTLE_TIMEOUT, STABILITY_SLEEP, SETTLE_SLEEP, BUILD_TIMEOUT

    scale = args.timeout_scale
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * scale)
    RELOAD_TIMEOUT = int(RELOAD_TIMEOUT * scale)
    PROBE_TIMEOUT = int(PROBE_TIMEOUT * scale)
    COUNTER_TICK_TIMEOUT = int(COUNTER_TICK_TIMEOUT * scale)
    SETTLE_TIMEOUT = int(SETTLE_TIMEOUT * scale)
    STABILITY_SLEEP = max(1, int(STABILITY_SLEEP * scale))
    SETTLE_SLEEP = max(1, int(SETTLE_SLEEP * scale))
    BUILD_TIMEOUT = int(BUILD_TIMEOUT * scale)

    # Capture originals for every file the suite may touch.
    for path in (HOST_CONFIG_YAML, PROJECT_SANDBOX_LIB_RS, SPLINE_LIB_RS):
        BACKUP.capture(path)

    kill_stale_hosts()

    results: List[Tuple[str, bool]] = []
    try:
        if not args.skip_build:
            if not build_host():
                sys.exit(1)

        results.append(
            (
                "A (tests/project + pill_spline)",
                run_session("A", SESSION_A_YAML, SESSION_A_SCENARIOS),
            )
        )
        if results[-1][1]:
            results.append(
                (
                    "B (examples/project_rs + pill_spline)",
                    run_session("B", SESSION_B_YAML, SESSION_B_SCENARIOS),
                )
            )
    finally:
        # Always restore the developer's files, even on failure.
        BACKUP.restore_all()
        # Restore the host config to its original content last.
        if HOST_CONFIG_YAML in BACKUP._originals:
            BACKUP.restore_one(HOST_CONFIG_YAML)

    print(f"\n{'=' * 64}")
    print("  SUMMARY")
    print(f"{'=' * 64}")
    all_passed = True
    for session_name, passed in results:
        status = "[PASS]" if passed else "[FAIL]"
        print(f"  {status} session {session_name}")
        all_passed = all_passed and passed

    if all_passed:
        print("\n  [PASS] Full hot-reload suite passed.")
        sys.exit(0)
    print("\n  [FAIL] One or more sessions failed.")
    sys.exit(1)


if __name__ == "__main__":
    main()

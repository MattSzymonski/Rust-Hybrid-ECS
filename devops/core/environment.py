"""
Git, operating system, CPU and toolchain metadata collection.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Builds the common metadata envelope every Pill Lab measurement carries.
    All collection here is best-effort: a missing `rustup`, an unavailable
    CIM provider or a checkout without git never fails a measurement, it only
    leaves a field out. That rule is deliberate - environment metadata is
    context, not a measurement precondition.

    `collect_system_info` is also imported by the legacy
    `modules/pill_engine/benches/reports/gen_bench_report.py`, so machine
    detection exists in exactly one place.

--- SCRIPT ---
"""

import os
import platform
import socket
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional

from .paths import REPOSITORY_ROOT, find_executable

# Every external probe is capped so a hung provider cannot stall a run.
PROBE_TIMEOUT_SECONDS = 8


def run_text_command(
    argv: List[str], cwd: Optional[str] = None, timeout: int = PROBE_TIMEOUT_SECONDS
) -> Optional[str]:
    """Runs a command and returns its trimmed stdout, or None on any failure.

    Used for every metadata probe, so a missing tool, a non-zero exit and a
    timeout all collapse to the same "field unavailable" outcome.
    """
    try:
        result = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd,
            encoding="utf-8",
            errors="replace",
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return (result.stdout or "").strip()


def local_timestamp() -> str:
    """Returns the current local time as an ISO-8601 string with UTC offset."""
    return datetime.now().astimezone().replace(microsecond=0).isoformat()


def filesystem_timestamp() -> str:
    """Returns a filesystem-safe local timestamp: `YYYY-MM-DD_HH-MM-SS`."""
    return datetime.now().astimezone().strftime("%Y-%m-%d_%H-%M-%S")


def utc_now_iso() -> str:
    """Returns the current UTC time as an ISO-8601 string."""
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


# =============================================================================
# Git metadata
# =============================================================================


def collect_git_metadata() -> Dict[str, Any]:
    """Collects commit, branch, dirty flag and subject for the working tree.

    Returns `{"available": False}` when the repository or git itself cannot be
    queried, so the frontend can say "no git data" instead of rendering blanks.
    """
    git = find_executable("git")
    working_directory = str(REPOSITORY_ROOT)
    commit = run_text_command([git, "rev-parse", "HEAD"], working_directory)
    if commit is None:
        return {"available": False}

    branch = run_text_command([git, "rev-parse", "--abbrev-ref", "HEAD"], working_directory)
    status = run_text_command([git, "status", "--porcelain"], working_directory)
    subject = run_text_command([git, "log", "-1", "--pretty=%s"], working_directory)
    commit_date = run_text_command([git, "log", "-1", "--pretty=%cI"], working_directory)
    dirty_lines = [line for line in (status or "").splitlines() if line.strip()]

    return {
        "available": True,
        "commit": commit,
        "commit_short": commit[:10],
        "branch": branch or "(detached)",
        # `git status --porcelain` prints one line per modified or untracked
        # path; empty output is the only definition of a clean tree.
        "dirty": bool(dirty_lines),
        "dirty_file_count": len(dirty_lines),
        "subject": subject or "",
        "commit_date": commit_date or "",
    }


# =============================================================================
# CPU detection
# =============================================================================


def _windows_cpu_details() -> Dict[str, Any]:
    """Queries CPU name, core counts and cache sizes through CIM, then WMIC.

    PowerShell/CIM is tried first because `wmic` is deprecated and missing on
    newer Windows installations; the WMIC branch remains as the fallback for
    older machines.
    """
    details: Dict[str, Any] = {}

    # One CIM query returns every field at once as `key=value` lines.
    powershell_script = (
        "$processor = Get-CimInstance Win32_Processor | Select-Object -First 1; "
        "Write-Output ('name=' + $processor.Name); "
        "Write-Output ('cores=' + $processor.NumberOfCores); "
        "Write-Output ('logical=' + $processor.NumberOfLogicalProcessors); "
        "Write-Output ('l2=' + $processor.L2CacheSize); "
        "Write-Output ('l3=' + $processor.L3CacheSize); "
        "Write-Output ('mhz=' + $processor.MaxClockSpeed)"
    )
    output = run_text_command(
        [
            find_executable("powershell"),
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            powershell_script,
        ]
    )
    for line in (output or "").splitlines():
        key, separator, value = line.partition("=")
        if not separator:
            continue
        key = key.strip()
        value = value.strip()
        if not value:
            continue
        if key == "name":
            details["cpu"] = value
        elif key == "cores" and value.isdigit():
            details["physical_cores"] = int(value)
        elif key == "logical" and value.isdigit():
            details["logical_cpus"] = int(value)
        elif key == "l2" and value.isdigit() and int(value) > 0:
            details["l2_cache_kb"] = int(value)
        elif key == "l3" and value.isdigit() and int(value) > 0:
            details["l3_cache_kb"] = int(value)
        elif key == "mhz" and value.isdigit():
            details["cpu_max_mhz"] = int(value)
    if details.get("cpu"):
        return details

    # WMIC fallback for machines without a usable PowerShell/CIM path.
    wmic_output = run_text_command(
        [
            find_executable("wmic"),
            "cpu",
            "get",
            "Name,NumberOfCores,NumberOfLogicalProcessors",
            "/format:list",
        ]
    )
    for line in (wmic_output or "").splitlines():
        key, separator, value = line.partition("=")
        if not separator:
            continue
        key = key.strip()
        value = value.strip()
        if not value:
            continue
        if key == "Name":
            details["cpu"] = value
        elif key == "NumberOfCores" and value.isdigit():
            details["physical_cores"] = int(value)
        elif key == "NumberOfLogicalProcessors" and value.isdigit():
            details["logical_cpus"] = int(value)
    return details


def _linux_cpu_details() -> Dict[str, Any]:
    """Parses `lscpu` for the CPU name, socket/core layout and cache sizes."""
    details: Dict[str, Any] = {}
    output = run_text_command([find_executable("lscpu")])
    if not output:
        return details
    cores_per_socket = 0
    sockets = 0
    for line in output.splitlines():
        key, separator, value = line.partition(":")
        if not separator:
            continue
        key = key.strip()
        value = value.strip()
        if key == "Model name":
            details["cpu"] = value
        elif key == "Core(s) per socket" and value.isdigit():
            cores_per_socket = int(value)
        elif key == "Socket(s)" and value.isdigit():
            sockets = int(value)
        elif key == "L2 cache":
            details["l2_cache"] = value
        elif key == "L3 cache":
            details["l3_cache"] = value
    if cores_per_socket and sockets:
        details["physical_cores"] = cores_per_socket * sockets
    return details


def _total_memory_gigabytes() -> Optional[float]:
    """Returns installed physical memory in GB, or None when undetectable."""
    if platform.system() == "Windows":
        try:
            import ctypes
            import ctypes.wintypes

            class MemoryStatusEx(ctypes.Structure):
                _fields_ = [
                    ("length", ctypes.wintypes.DWORD),
                    ("memory_load", ctypes.wintypes.DWORD),
                    ("total_physical", ctypes.c_uint64),
                    ("available_physical", ctypes.c_uint64),
                    ("total_page_file", ctypes.c_uint64),
                    ("available_page_file", ctypes.c_uint64),
                    ("total_virtual", ctypes.c_uint64),
                    ("available_virtual", ctypes.c_uint64),
                    ("available_extended_virtual", ctypes.c_uint64),
                ]

            memory_status = MemoryStatusEx()
            memory_status.length = ctypes.sizeof(MemoryStatusEx)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(memory_status)):
                return round(memory_status.total_physical / (1024 ** 3), 1)
        except Exception:
            return None
        return None
    try:
        total_bytes = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")
        return round(total_bytes / (1024 ** 3), 1)
    except (AttributeError, ValueError, OSError):
        return None


# =============================================================================
# Environment envelope
# =============================================================================


def collect_environment() -> Dict[str, Any]:
    """Collects the OS / CPU / toolchain block stored with every measurement.

    Only fields that actually resolved are present, so the frontend renders
    whatever it finds without having to filter placeholder strings.
    """
    environment: Dict[str, Any] = {
        "os": platform.system() + " " + platform.release(),
        "os_version": platform.version(),
        "architecture": platform.machine(),
        "hostname": socket.gethostname(),
        "python": sys.version.split()[0],
        "logical_cpus": os.cpu_count() or 0,
    }

    if platform.system() == "Windows":
        environment.update(_windows_cpu_details())
    elif platform.system() == "Linux":
        environment.update(_linux_cpu_details())
    environment.setdefault("cpu", platform.processor() or "Unknown")

    memory_gigabytes = _total_memory_gigabytes()
    if memory_gigabytes is not None:
        environment["ram_gb"] = memory_gigabytes

    rustc_version = run_text_command([find_executable("rustc"), "--version"])
    if rustc_version:
        environment["rustc"] = rustc_version
    cargo_version = run_text_command([find_executable("cargo"), "--version"])
    if cargo_version:
        environment["cargo"] = cargo_version
    toolchain = run_text_command([find_executable("rustup"), "show", "active-toolchain"])
    if toolchain:
        # `rustup show active-toolchain` can print an explanatory second line.
        environment["active_toolchain"] = toolchain.splitlines()[0].strip()

    verbose_rustc = run_text_command([find_executable("rustc"), "--version", "--verbose"])
    for line in (verbose_rustc or "").splitlines():
        if line.startswith("host:"):
            environment["host_triple"] = line.split(":", 1)[1].strip()
        elif line.startswith("commit-hash:"):
            environment["rustc_commit"] = line.split(":", 1)[1].strip()[:10]

    dotnet_version = run_text_command([find_executable("dotnet"), "--version"])
    if dotnet_version:
        environment["dotnet"] = dotnet_version

    return environment


def collect_system_info() -> Dict[str, str]:
    """Returns the environment as the flat label -> value map the legacy
    `gen_bench_report.py` HTML table expects.

    Defined here so machine detection has one implementation; the legacy
    report generator imports this instead of probing the machine itself.
    """
    environment = collect_environment()
    label_map = [
        ("os", "OS"),
        ("os_version", "OS Version"),
        ("architecture", "Architecture"),
        ("cpu", "CPU"),
        ("physical_cores", "Physical Cores"),
        ("logical_cpus", "Logical Processors"),
        ("cpu_max_mhz", "Max Clock (MHz)"),
        ("l2_cache_kb", "L2 Cache (KB)"),
        ("l3_cache_kb", "L3 Cache (KB)"),
        ("l2_cache", "L2 Cache"),
        ("l3_cache", "L3 Cache"),
        ("ram_gb", "RAM (GB)"),
        ("host_triple", "Host Triple"),
        ("rustc", "rustc"),
        ("rustc_commit", "rustc Commit"),
        ("cargo", "cargo"),
        ("active_toolchain", "Active Toolchain"),
        ("dotnet", ".NET SDK"),
        ("hostname", "Hostname"),
        ("python", "Python"),
    ]
    return {
        label: str(environment[key]) for key, label in label_map if key in environment
    }

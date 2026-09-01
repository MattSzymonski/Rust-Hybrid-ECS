# Standard library
import os
import sys
from pathlib import Path

sys.path.insert(0, r"d:\Programming\Rust-Hybrid-ECS\devops\ci_cd")
import build_release

calls = []


def fake_run(command, cwd=None, env=None):
    calls.append({"command": command, "rustflags": (env or {}).get("RUSTFLAGS")})
    return type("Completed", (), {"returncode": 0})()


build_release.subprocess.run = fake_run
os.environ["PROJECT_PATH"] = "examples/project_rs"

build_release.sys.argv = ["build_release.py", "--features", "rendering"]
code = build_release.main()
cargo = next((c for c in calls if c["command"][0] == "cargo"), None)
print("exit:", code)
print("cargo args:", cargo["command"] if cargo else None)

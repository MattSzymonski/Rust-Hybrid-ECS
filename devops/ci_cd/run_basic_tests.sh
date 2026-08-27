#!/usr/bin/env bash

# REQUIREMENTS: bash 4+, Python 3.8+, Rust toolchain (cargo, rustfmt, clippy)
#               on PATH. The launcher-driven checks additionally need a
#               compiled PillLauncher binary (auto-discovered, or set via
#               PILL_LAUNCHER_BIN) and the example projects they build; when
#               those are absent the checks report SKIP rather than failing.

# DESCRIPTION: Thin runner for the Pill CI fast checks. All of the logic lives
#   in devops/tests/test_basic.py; this script only locates the repository
#   root and forwards its arguments, so the checks behave identically whether
#   they are invoked here or directly:
#
#     python devops/tests/test_basic.py code_linting
#
#   Checks: code formatting (cargo fmt), code linting (cargo clippy), the Rust
#   test suite in both feature configurations, the static shipping build with
#   proof that the hot-reload machinery is compiled out, native example build
#   with an artifact size report, WASM build with a size budget and a
#   dev-server smoke test, and the native performance benchmark.
#
#   Designed for both local development and GitHub Actions CI
#   (ci-basic-tests.yml).

# USAGE: bash devops/ci_cd/run_basic_tests.sh [all|<check-name>] [--list]
#
#   all                            run every check (default)
#   code_formatting                cargo fmt --check over the workspace
#   code_linting                   cargo clippy -D warnings over the workspace
#   rust_tests                     cargo test --workspace, with and without hot_patch
#   shipping_build                 static release build + proof the reload
#                                  machinery is gone
#   native_example_build           launcher release build + artifact sizes
#   wasm_example_build             launcher WASM build + size budget + smoke test
#   native_performance_benchmark   build + run the benchmark project (release)
#   --list                         print the available checks, then exit
#
#   Every argument is passed straight through to the Python script.

# EXAMPLE USAGE:
#   bash devops/ci_cd/run_basic_tests.sh all
#   bash devops/ci_cd/run_basic_tests.sh code_linting
#   bash devops/ci_cd/run_basic_tests.sh --list
#
# Exit status: 0 when every check passed or skipped, 1 when any failed, 2 on
# a usage error.

# --- SCRIPT ---

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# The repository root is two levels up: devops/ci_cd -> devops -> root.
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Shortcuts for colored section headers
BOLD='\033[1m'
CYAN='\033[0;36m'
RED='\033[0;31m'
NC='\033[0m'

# All paths passed to the checks are relative to the project root.
cd "$PROJECT_ROOT"

# Force colored output from cargo and git even when piped (Docker/TTY-less).
export CARGO_TERM_COLOR=always
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=color.ui
export GIT_CONFIG_VALUE_0=always

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    sed -n '/^# USAGE/,/^# --- SCRIPT ---/p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
echo -e "${BOLD}${CYAN}Pill CI fast checks${NC}"
echo ""

set +e
python devops/tests/test_basic.py "$@"
checks_exit=$?
set -e

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
if [ "$checks_exit" -eq 0 ]; then
    echo -e "${BOLD}${CYAN}Basic checks PASSED${NC}"
else
    echo -e "${BOLD}${RED}Basic checks FAILED (exit $checks_exit)${NC}"
fi
echo -e "${BOLD}${CYAN}===============================================================================${NC}"

exit "$checks_exit"

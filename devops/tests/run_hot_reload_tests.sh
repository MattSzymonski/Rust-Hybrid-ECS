#!/usr/bin/env bash

# REQUIREMENTS: Python 3.8+, Rust toolchain (cargo) on PATH. The suite
#               launches the standalone host and drives real source edits, so
#               it needs a working native toolchain (it builds
#               `pill_standalone` first unless --skip-build is given).

# DESCRIPTION: Run the full hot-reload integration suite
#   (tests/test_hot_reload_suite.py). The suite launches the standalone host,
#   drives live source edits against a real project and an optional module,
#   and asserts the host's reload behaviour (reloads, schema migration,
#   forgotten-type detection, rollback, cascade) from its console output.
#
#   The suite temporarily modifies - and automatically restores - the files
#   modules/pill_config.yaml, tests/project/src/lib.rs and
#   modules/optional/pill_spline/src/lib.rs, so a normal developer workspace
#   is left exactly as it was.
#
#   Expected runtime: ~3-5 minutes (plus the initial host build unless
#   --skip-build is used).
#
#   Designed for both local development and GitHub Actions CI.

# USAGE: bash devops/tests/run_hot_reload_tests.sh [--skip-build] [--timeout-scale S]
#
#   --skip-build          skip the initial host build (assume pill_standalone
#                         is already built; fastest lane for CI)
#   --timeout-scale S     multiply all suite timeouts by S (default 1.5)

# EXAMPLE USAGE:
#   bash devops/tests/run_hot_reload_tests.sh
#   bash devops/tests/run_hot_reload_tests.sh --skip-build
#   bash devops/tests/run_hot_reload_tests.sh --timeout-scale 2.0

# --- SCRIPT ---

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../common.sh
source "$SCRIPT_DIR/../common.sh"

# Shortcuts for colored section headers
BOLD='\033[1m'
CYAN='\033[0;36m'
NC='\033[0m'

# All paths in this script are relative to the project root.
cd "$PROJECT_ROOT"

SKIP_BUILD=0
TIMEOUT_SCALE=1.5

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --timeout-scale)
            TIMEOUT_SCALE="$2"
            shift 2
            ;;
        --timeout-scale=*)
            TIMEOUT_SCALE="${1#*=}"
            shift
            ;;
        -h|--help)
            sed -n '/^# USAGE/,/^# --- SCRIPT ---/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument '$1'" >&2
            exit 2
            ;;
    esac
done

case "$TIMEOUT_SCALE" in
    ''|*[!0-9.]*) echo "ERROR: --timeout-scale must be a number (got '$TIMEOUT_SCALE')" >&2; exit 2 ;;
esac

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
echo -e "${BOLD}${CYAN}Hot-reload integration suite (tests/test_hot_reload_suite.py)${NC}"
echo "  timeout scale:  ${TIMEOUT_SCALE}"
if [ "$SKIP_BUILD" = 1 ]; then
    echo "  host build:     skipped (assumed up to date)"
    extra_args=(--skip-build)
else
    echo "  host build:     yes"
    extra_args=()
fi
echo ""

# Run the suite. set -e is suspended so the exit code can be reported and
# propagated explicitly.
set +e
python tests/test_hot_reload_suite.py --timeout-scale "$TIMEOUT_SCALE" "${extra_args[@]}"
suite_exit=$?
set -e

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
if [ "$suite_exit" -eq 0 ]; then
    echo -e "${BOLD}${CYAN}Hot-reload suite PASSED${NC}"
else
    echo -e "${BOLD}${RED}Hot-reload suite FAILED (exit $suite_exit)${NC}"
fi
echo -e "${BOLD}${CYAN}===============================================================================${NC}"

exit "$suite_exit"

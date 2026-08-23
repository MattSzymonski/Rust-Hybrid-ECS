#!/usr/bin/env bash

# REQUIREMENTS: Python 3.8+, Rust toolchain (cargo) on PATH. The suite
#               launches the standalone host and drives real source edits, so
#               it needs a working native toolchain (it builds
#               `pill_standalone` first unless --skip-build is given).

# DESCRIPTION: Run the full hot-reload regression net
#   (1) tests/test_hot_reload_suite.py            - full suite (sessions A/B:
#        reloads, schema migration, forgotten-type detection, drop-at-detection
#        re-seed, repeated-reload stability, rollback, cascade, coexistence)
#   (2) tests/test_hot_reload_migration.py        - table-driven migration suite
#        (fast path, add/revert fields, rename field, downgrade)
#   (3) tests/test_module_project_auto_reload.py  - module->project cascade
#   (4) tests/test_csharp_bridge.py               - C# <-> Rust bridge suite
#        (managed backend startup, codegen mirror content, empty-exposure
#        handling, clean managed build, Rust->C# and C#->Rust bridge probes,
#        behavior-only C# hot reload, mirror regeneration on restart)
#
#   Each suite launches the standalone host and drives live source edits
#   against a real project and an optional module, asserting the host's reload
#   behaviour from its console output. The suites temporarily modify - and
#   automatically restore - modules/pill_config.yaml, tests/project/src/lib.rs,
#   examples/project_rs/src/lib.rs, examples/project_cs/src/Systems.cs and
#   modules/optional/pill_spline/src/lib.rs, so a normal developer workspace
#   is left exactly as it was.
#
#   Expected runtime: ~10-15 minutes (plus the initial host build unless
#   --skip-build is used).
#
#   NOTE: suite (4) needs the .NET SDK on PATH (the host runs `dotnet build`
#   for the managed project).
#
#   Designed for both local development and GitHub Actions CI.

# USAGE: bash devops/tests/run_hot_reload_tests.sh [--skip-build] [--timeout-scale S]
#
#   --skip-build          skip the initial host build (assume pill_standalone
#                         is already built; fastest lane for CI). Only the
#                         main suite honours it; the migration and cascade
#                         suites always build what they need.
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
echo -e "${BOLD}${CYAN}Hot-reload regression net (4 suites)${NC}"
echo "  timeout scale:  ${TIMEOUT_SCALE}"
if [ "$SKIP_BUILD" = 1 ]; then
    echo "  host build:     skipped (assumed up to date; main suite only)"
else
    echo "  host build:     yes"
fi
echo ""

overall_exit=0

# --- 1. Main hot-reload suite (sessions A/B) ---------------------------------
echo -e "${BOLD}${CYAN}--- Suite 1/3: test_hot_reload_suite.py ---${NC}"
if [ "$SKIP_BUILD" = 1 ]; then
    extra_args=(--skip-build)
else
    extra_args=()
fi
set +e
python tests/test_hot_reload_suite.py --timeout-scale "$TIMEOUT_SCALE" "${extra_args[@]}"
suite_exit=$?
set -e
if [ "$suite_exit" -ne 0 ]; then
    overall_exit="$suite_exit"
fi

# --- 2. Migration suite -------------------------------------------------------
echo ""
echo -e "${BOLD}${CYAN}--- Suite 2/3: test_hot_reload_migration.py ---${NC}"
set +e
python tests/test_hot_reload_migration.py --timeout-scale "$TIMEOUT_SCALE"
migration_exit=$?
set -e
if [ "$migration_exit" -ne 0 ]; then
    overall_exit="$migration_exit"
fi

# --- 3. Module->project cascade suite -----------------------------------------
echo ""
echo -e "${BOLD}${CYAN}--- Suite 3/3: test_module_project_auto_reload.py ---${NC}"
set +e
python tests/test_module_project_auto_reload.py --timeout-scale "$TIMEOUT_SCALE"
cascade_exit=$?
set -e
if [ "$cascade_exit" -ne 0 ]; then
    overall_exit="$cascade_exit"
fi

# --- 4. C# <-> Rust bridge suite ---------------------------------------------
echo ""
echo -e "${BOLD}${CYAN}--- Suite 4/4: test_csharp_bridge.py ---${NC}"
set +e
python tests/test_csharp_bridge.py --timeout-scale "$TIMEOUT_SCALE" "${extra_args[@]}"
csharp_exit=$?
set -e
if [ "$csharp_exit" -ne 0 ]; then
    overall_exit="$csharp_exit"
fi

echo ""
echo -e "${BOLD}${CYAN}===============================================================================${NC}"
if [ "$overall_exit" -eq 0 ]; then
    echo -e "${BOLD}${CYAN}Hot-reload regression net PASSED (all 4 suites)${NC}"
else
    echo -e "${BOLD}${RED}Hot-reload regression net FAILED (exit $overall_exit)${NC}"
fi
echo -e "${BOLD}${CYAN}===============================================================================${NC}"

exit "$overall_exit"

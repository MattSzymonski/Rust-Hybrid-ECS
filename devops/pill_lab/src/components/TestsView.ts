// DESCRIPTION: The Tests tab, rendered into the same two-column shell as the
//   Benchmarks tab. The left sidebar lists every suite from `devops/tests/`
//   (with a status dot per run state); the main column shows the selected
//   suite's full name, description, current state, its per-step spinners, and
//   a live terminal that pins to the bottom as the output streams in.
//
//   Run state is module-level so a run keeps progressing (and re-renders)
//   when the user switches to Benchmarks and comes back.
//
// --- SCRIPT ---

import { clear, element } from '../lib/dom';
import { listTests, startTest } from '../lib/testRunner';
import type { TestInfo, TestRunState } from '../types/test';

/** One stored run, keyed by test name. */
interface RunEntry {
  state: TestRunState;
  /** Whether the live terminal is folded; survives re-renders. */
  terminalCollapsed: boolean;
}

function idleState(): TestRunState {
  return { phase: 'idle', steps: [], logs: [], exitCode: null };
}

// -----------------------------------------------------------------------------
// Module-level store
// -----------------------------------------------------------------------------

let knownTests: TestInfo[] | null = null;
let testsLoadError: string | null = null;
const runs = new Map<string, RunEntry>();
let activeName: string | null = null;
let selectedName: string | null = null;
let sidebarHost: HTMLElement | null = null;
let mainHost: HTMLElement | null = null;
let draw: (() => void) | null = null;

function runOf(name: string): RunEntry {
  let run = runs.get(name);
  if (!run) {
    run = { state: idleState(), terminalCollapsed: true };
    runs.set(name, run);
  }
  return run;
}

// -----------------------------------------------------------------------------
// Entry point
// -----------------------------------------------------------------------------

/** Renders (or refreshes) the whole Tests tab into the two column hosts. */
export async function renderTestsApp(sidebar: HTMLElement, main: HTMLElement): Promise<void> {
  sidebarHost = sidebar;
  mainHost = main;
  if (knownTests === null && testsLoadError === null) {
    try {
      knownTests = await listTests();
    } catch (error) {
      testsLoadError = (error as Error).message;
    }
    // Select the first suite so the main column is never empty.
    if (!selectedName && knownTests && knownTests.length > 0) {
      selectedName = knownTests[0].name;
    }
  }
  draw = () => {
    drawSidebar();
    drawMain();
  };
  draw();
}

// -----------------------------------------------------------------------------
// Sidebar: the suite list
// -----------------------------------------------------------------------------

/** Short status word shown on the right of each suite row (blank when idle). */
function phaseText(phase: TestRunState['phase']): string {
  switch (phase) {
    case 'running':
      return 'running';
    case 'passed':
      return 'passed';
    case 'failed':
      return 'failed';
    case 'error':
      return 'error';
    default:
      return '';
  }
}

function drawSidebar(): void {
  if (!sidebarHost) return;
  clear(sidebarHost);

  const section = element('div', { class: 'sidebar-section' });
  section.appendChild(element('div', { class: 'sidebar-section-label', text: 'Tests' }));

  if (testsLoadError) {
    section.appendChild(element('p', { class: 'tests-note', text: testsLoadError }));
    sidebarHost.appendChild(section);
    return;
  }
  if (!knownTests || knownTests.length === 0) {
    section.appendChild(
      element('p', {
        class: 'tests-note',
        text: 'No test_*.py suites found under devops/tests/.',
      }),
    );
    sidebarHost.appendChild(section);
    return;
  }

  const runAll = element(
    'button',
    {
      class: 'test-run-button primary tests-run-all',
      type: 'button',
      disabled: activeName !== null ? 'true' : undefined,
      text: activeName ? 'Running…' : 'Run all',
    },
  );
  runAll.addEventListener('click', () => {
    void runAllTests();
  });
  section.appendChild(element('div', { class: 'tests-sidebar-actions' }, runAll));

  const picker = element('div', { class: 'tests-picker' });
  for (const test of knownTests) {
    const run = runOf(test.name);
    const classes = ['test-row'];
    if (test.name === selectedName) classes.push('active');
    if (run.state.phase !== 'idle') classes.push(`phase-${run.state.phase}`);

    const row = element(
      'button',
      { class: classes.join(' '), type: 'button' },
      element('span', { class: 'test-row-main' },
        element('span', { class: `test-status-dot ${run.state.phase}` }),
        element('span', { class: 'test-row-name', text: test.name }),
      ),
      element('span', { class: 'test-row-phase', text: phaseText(run.state.phase) }),
    );
    row.addEventListener('click', () => {
      selectedName = test.name;
      draw?.();
    });
    picker.appendChild(row);
  }
  section.appendChild(picker);
  sidebarHost.appendChild(section);
}

// -----------------------------------------------------------------------------
// Main: the selected suite's detail + terminal
// -----------------------------------------------------------------------------

function drawMain(): void {
  if (!mainHost) return;
  clear(mainHost);

  if (testsLoadError) {
    mainHost.appendChild(
      element('div', { class: 'empty-state error-state' },
        element('h2', { text: 'Tests unavailable' }),
        element('p', { text: testsLoadError }),
      ),
    );
    return;
  }
  if (!knownTests || knownTests.length === 0) {
    mainHost.appendChild(
      element('div', { class: 'empty-state' },
        element('h2', { text: 'No test suites found' }),
        element('p', { text: 'No test_*.py suites were found under devops/tests/.' }),
      ),
    );
    return;
  }

  const test = knownTests.find((candidate) => candidate.name === selectedName) ?? knownTests[0];
  const run = runOf(test.name);
  const isActive = activeName === test.name;

  const detail = element('div', { class: 'tests-detail' });

  // Header: full name, title, run button.
  const runButton = element(
    'button',
    {
      class: `test-run-button ${run.state.phase === 'idle' ? 'primary' : ''}`,
      type: 'button',
      disabled: activeName !== null && !isActive ? 'true' : undefined,
      text: isActive ? 'Running…' : run.state.phase === 'idle' ? 'Run' : 'Run again',
    },
  );
  runButton.addEventListener('click', () => startRun(test.name));

  const header = element(
    'div',
    { class: 'tests-detail-header' },
    element('div', { class: 'tests-detail-titles' },
      element('h1', { text: test.title || test.name }),
      element('code', { class: 'test-file', text: test.name }),
    ),
    runButton,
  );
  detail.appendChild(header);

  // State chips - shown right under the header so the status is visible
  // before the description, matching the Benchmarks head layout.
  const chips = element('div', { class: 'tests-state-chips' });
  chips.appendChild(stateChip('state', run.state.phase));
  if (run.state.exitCode !== null) {
    chips.appendChild(stateChip('exit', String(run.state.exitCode)));
  }
  if (run.state.error) {
    chips.appendChild(element('span', { class: 'meta-chip test-error-chip', text: 'error' }));
  }
  detail.appendChild(chips);

  if (test.description) {
    detail.appendChild(element('p', { class: 'test-description', text: test.description }));
  }

  // Steps (spinning circles).
  if (run.state.steps.length > 0) {
    detail.appendChild(buildSteps(run.state));
  }

  // Outcome banner.
  if (run.state.phase === 'passed' || run.state.phase === 'failed') {
    detail.appendChild(
      element('div', { class: `test-outcome ${run.state.phase}` },
        element('span', { text: run.state.phase === 'passed' ? 'PASSED' : 'FAILED' }),
        run.state.exitCode !== null
          ? element('span', { class: 'mono', text: `exit code ${run.state.exitCode}` })
          : null,
      ),
    );
  }

  // Terminal: the live log, folded by default, pinned to the bottom when open.
  detail.appendChild(buildTerminal(test, run));

  mainHost.appendChild(detail);
}

function stateChip(key: string, value: string): HTMLElement {
  return element(
    'span',
    { class: `meta-chip state-${value}` },
    element('span', { class: 'meta-key', text: key }),
    element('span', { text: value }),
  );
}

function buildSteps(state: TestRunState): HTMLElement {
  const list = element('ol', { class: 'test-steps' });
  for (const step of state.steps) {
    list.appendChild(
      element(
        'li',
        { class: `test-step ${step.state}` },
        element('span', { class: 'test-step-indicator' }),
        element('span', { class: 'test-step-label', text: step.label }),
      ),
    );
  }
  return list;
}

function buildTerminal(test: TestInfo, run: RunEntry): HTMLElement {
  const state = run.state;
  const terminal = element(
    'div',
    { class: `tests-terminal${run.terminalCollapsed ? ' collapsed' : ''}` },
    element('div', { class: 'tests-terminal-head' },
      element('span', { class: 'tests-terminal-head-left' },
        element('span', { class: 'tests-terminal-toggle', text: '▾' }),
        element('span', { text: `Terminal — ${test.name}` }),
      ),
      element('span', {
        class: `tests-terminal-dot ${state.phase}`,
        text: state.phase === 'idle' ? 'idle' : state.phase === 'running' ? 'running' : 'ended',
      }),
    ),
    element('pre', { class: 'tests-terminal-body' }),
  );
  const body = terminal.querySelector('pre') as HTMLElement;
  body.textContent = state.logs.join('\n');
  // Pin to the bottom so the newest lines are always visible.
  body.scrollTop = body.scrollHeight;
  // Clicking the header folds/unfolds the log; the choice survives re-renders.
  terminal.querySelector('.tests-terminal-head')?.addEventListener('click', () => {
    run.terminalCollapsed = !run.terminalCollapsed;
    terminal.classList.toggle('collapsed');
  });
  return terminal;
}

// -----------------------------------------------------------------------------
// Running
// -----------------------------------------------------------------------------

function startRun(name: string): void {
  if (activeName) return; // one test at a time
  const run = runOf(name);
  run.state = { phase: 'running', steps: [], logs: [], exitCode: null };
  activeName = name;
  selectedName = name;
  draw?.();

  startTest(name, {
    onState: (state) => {
      run.state = state;
      if (state.phase === 'passed' || state.phase === 'failed' || state.phase === 'error') {
        activeName = null;
      }
      draw?.();
    },
  });
}

/** Runs every suite in sequence, waiting for each to finish. */
async function runAllTests(): Promise<void> {
  if (!knownTests || activeName) return;
  for (const test of knownTests) {
    if (activeName) {
      // Another tab/run started meanwhile; wait for it before continuing.
      await waitForIdle();
    }
    startRun(test.name);
    await waitForCompletion(test.name);
  }
}

function waitForIdle(): Promise<void> {
  return new Promise((resolve) => {
    const poll = (): void => {
      if (!activeName) resolve();
      else setTimeout(poll, 200);
    };
    poll();
  });
}

function waitForCompletion(name: string): Promise<void> {
  return new Promise((resolve) => {
    const poll = (): void => {
      const run = runs.get(name);
      const phase = run?.state.phase;
      if (!phase || phase === 'idle' || phase === 'running') {
        setTimeout(poll, 200);
      } else {
        resolve();
      }
    };
    poll();
  });
}

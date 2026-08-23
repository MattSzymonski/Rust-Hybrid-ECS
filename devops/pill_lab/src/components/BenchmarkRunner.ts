// DESCRIPTION: Benchmark runs, split across the same two columns as the
//   Benchmarks report. The sidebar section shows only the currently selected
//   category's run row (a status dot + label + phase); the main panel carries
//   the actual Run button (top-right, mirroring the Tests tab), state chips,
//   and a live terminal that pins to the bottom as the output streams in.
//
//   Run state is module-level so it survives re-renders and tab switches.
//
// --- SCRIPT ---

import { clear, element } from '../lib/dom';
import {
  listBenchmarks,
  startBenchmark,
  type BenchmarkInfo,
  type BenchmarkRunState,
} from '../lib/benchmarkRunner';

interface RunEntry {
  state: BenchmarkRunState;
  /** Whether the live terminal is folded; survives re-renders. */
  terminalCollapsed: boolean;
}

function idleState(): BenchmarkRunState {
  return { phase: 'idle', exitCode: null, logs: [] };
}

// -----------------------------------------------------------------------------
// Module-level store
// -----------------------------------------------------------------------------

let knownBenchmarks: BenchmarkInfo[] | null = null;
const runs = new Map<string, RunEntry>();
let activeCategory: string | null = null;
let selectedCategory: string | null = null;
let sidebarHost: HTMLElement | null = null;
let panelHost: HTMLElement | null = null;
let onCompleted: (() => void) | null = null;
let draw: (() => void) | null = null;

function runOf(category: string): RunEntry {
  let run = runs.get(category);
  if (!run) {
    run = { state: idleState(), terminalCollapsed: true };
    runs.set(category, run);
  }
  return run;
}

function benchmarkLabel(category: string): string {
  return knownBenchmarks?.find((candidate) => candidate.category === category)?.label ?? category;
}

// -----------------------------------------------------------------------------
// Entry point
// -----------------------------------------------------------------------------

/**
 * Renders the benchmark-run UI into the two hosts: the sidebar row and the
 * main panel (Run button + chips + terminal). Called on every render so a
 * category change re-targets the single sidebar row.
 */
export async function renderBenchmarksRuns(
  sidebar: HTMLElement,
  panel: HTMLElement,
  category: string,
  completed: () => void,
): Promise<void> {
  sidebarHost = sidebar;
  panelHost = panel;
  onCompleted = completed;
  selectedCategory = category;
  if (knownBenchmarks === null) {
    try {
      knownBenchmarks = await listBenchmarks();
    } catch {
      knownBenchmarks = [];
    }
  }
  draw = () => {
    drawSidebar();
    drawPanel();
  };
  draw();
}

// -----------------------------------------------------------------------------
// Sidebar: one row for the selected category
// -----------------------------------------------------------------------------

function drawSidebar(): void {
  if (!sidebarHost) return;
  clear(sidebarHost);

  const section = element('div', { class: 'sidebar-section' });
  section.appendChild(element('div', { class: 'sidebar-section-label', text: 'Run benchmark' }));

  if (!knownBenchmarks || knownBenchmarks.length === 0 || !selectedCategory) {
    section.appendChild(
      element('p', {
        class: 'benchmark-run-note',
        text: 'Benchmarks run through the dev server (pill_lab.py serve).',
      }),
    );
    sidebarHost.appendChild(section);
    return;
  }

  const run = runOf(selectedCategory);
  const phase = run.state.phase;
  const phaseText =
    phase === 'idle'
      ? 'not run'
      : phase === 'running'
        ? 'running…'
        : phase === 'passed'
          ? 'done'
          : phase === 'failed'
            ? 'failed'
            : 'error';

  section.appendChild(
    element(
      'div',
      { class: 'benchmark-run-row' },
      element('span', { class: `test-status-dot ${phase}` }),
      element('span', { class: 'benchmark-run-label', text: benchmarkLabel(selectedCategory) }),
      element('span', { class: 'benchmark-run-phase', text: phaseText }),
    ),
  );
  sidebarHost.appendChild(section);
}

// -----------------------------------------------------------------------------
// Main panel: Run button (top-right), chips, terminal
// -----------------------------------------------------------------------------

function drawPanel(): void {
  if (!panelHost) return;
  clear(panelHost);

  const category = selectedCategory;
  if (!category) return;
  const run = runOf(category);
  const isActive = activeCategory === category;

  const runButton = element(
    'button',
    {
      class: `test-run-button ${run.state.phase === 'idle' ? 'primary' : ''}`,
      type: 'button',
      disabled: activeCategory !== null && !isActive ? 'true' : undefined,
      text: isActive
        ? 'Running…'
        : run.state.phase === 'idle'
          ? 'Run'
          : 'Run again',
    },
  );
  runButton.addEventListener('click', () => startRun(category));

  const chips = element('div', { class: 'tests-state-chips' });
  chips.appendChild(stateChip('state', run.state.phase));
  if (run.state.exitCode !== null) {
    chips.appendChild(stateChip('exit', String(run.state.exitCode)));
  }
  if (run.state.error) {
    chips.appendChild(
      element('span', { class: 'meta-chip test-error-chip', text: 'error' }),
    );
  }

  const panel = element(
    'div',
    { class: 'benchmark-run-panel' },
    element(
      'div',
      { class: 'benchmark-run-head' },
      element('div', { class: 'benchmark-run-titles' },
        element('span', { class: 'benchmark-run-title', text: benchmarkLabel(category) }),
        chips,
      ),
      runButton,
    ),
  );

  // The live terminal is always visible, mirroring the Tests tab - it shows
  // the idle state before a run and streams output while one is running.
  panel.appendChild(buildTerminal(benchmarkLabel(category), run));
  if (run.state.phase === 'passed' || run.state.phase === 'failed') {
    panel.appendChild(
      element('div', { class: `test-outcome ${run.state.phase}` },
        element('span', { text: run.state.phase === 'passed' ? 'PASSED' : 'FAILED' }),
        run.state.exitCode !== null
          ? element('span', { class: 'mono', text: `exit code ${run.state.exitCode}` })
          : null,
      ),
    );
  }

  panelHost.appendChild(panel);
}

function stateChip(key: string, value: string): HTMLElement {
  return element(
    'span',
    { class: `meta-chip state-${value}` },
    element('span', { class: 'meta-key', text: key }),
    element('span', { text: value }),
  );
}

function buildTerminal(label: string, run: RunEntry): HTMLElement {
  const state = run.state;
  const terminal = element(
    'div',
    { class: `tests-terminal${run.terminalCollapsed ? ' collapsed' : ''}` },
    element('div', { class: 'tests-terminal-head' },
      element('span', { class: 'tests-terminal-head-left' },
        element('span', { class: 'tests-terminal-toggle', text: '▾' }),
        element('span', { text: `Terminal — ${label}` }),
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

function startRun(category: string): void {
  if (activeCategory) return; // one run at a time
  const run = runOf(category);
  run.state = { phase: 'running', exitCode: null, logs: [] };
  activeCategory = category;
  draw?.();

  startBenchmark(category, {
    onState: (state) => {
      run.state = state;
      if (state.phase === 'passed' || state.phase === 'failed' || state.phase === 'error') {
        activeCategory = null;
      }
      draw?.();
      if (state.phase === 'passed') onCompleted?.();
    },
  });
}

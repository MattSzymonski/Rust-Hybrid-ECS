// DESCRIPTION: The Hot Reloading view. Each measured category becomes a card
//   showing its wall-time summary, the host's own build/stage/load/init/migrate
//   phase split as a proportional bar, the per-iteration table and the crates
//   cargo actually rebuilt.
//
//   Phase data comes from the engine host's `[analytics] reload` line, so a
//   card only shows phases the host actually reported - the C# path emits no
//   analytics and is therefore wall-time only, stated explicitly rather than
//   padded with zeros.
//
// --- SCRIPT ---

import type { Chart } from 'chart.js';
import { createBarChart } from '../lib/charts';
import { computeDelta, indexBy, type Delta } from '../lib/compare';
import { element, scrollToOnClick, tableHead, tableRow } from '../lib/dom';
import { formatMilliseconds, formatRatioPercent, humanizeIdentifier } from '../lib/format';
import type {
  HotReloadCase,
  HotReloadMeasurement,
  HotReloadSession,
} from '../types/measurement';
import type { ReportView } from './ReportView';

/** Phase order and the CSS variable each phase is drawn with. */
const PHASES: { key: keyof PhaseValues; label: string; variable: string }[] = [
  { key: 'build_ms', label: 'build', variable: '--phase-build' },
  { key: 'stage_ms', label: 'stage', variable: '--phase-stage' },
  { key: 'load_ms', label: 'load', variable: '--phase-load' },
  { key: 'init_ms', label: 'init', variable: '--phase-init' },
  { key: 'migrate_ms', label: 'migrate', variable: '--phase-migrate' },
];

type PhaseValues = Partial<
  Record<'build_ms' | 'stage_ms' | 'load_ms' | 'init_ms' | 'migrate_ms', number>
>;

/** Reads a CSS custom property so charts and bars share one palette. */
function cssColor(variable: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(variable).trim() || '#888';
}

export function renderHotReloadReport(
  measurement: HotReloadMeasurement,
  baseline: HotReloadMeasurement | null,
): ReportView {
  const payload = measurement.measurement;
  const baselineCases = baseline
    ? indexBy(baseline.measurement.cases, (entry) => entry.name)
    : new Map<string, HotReloadCase>();

  const content = element('div');
  const charts: Chart[] = [];

  content.appendChild(renderSummaryTiles(payload.cases, baselineCases));

  content.appendChild(
    element('p', {
      class: 'section-hint',
      text:
        `${payload.iterations} measured iteration(s) per category` +
        `${payload.warmup ? ' after a warmup edit/restore' : ' with no warmup'}. ` +
        payload.wall_time_definition,
    }),
  );

  const startupSection = renderStartupSection(payload.sessions, baseline);
  if (startupSection) content.appendChild(startupSection);

  // Overview chart: every category's average wall time, current versus
  // baseline when one is selected.
  const overview = element('div', { class: 'section' });
  overview.appendChild(element('h2', { text: 'Reload latency by category' }));
  overview.appendChild(
    element('p', { class: 'section-hint', text: 'Average wall time, lower is better.' }),
  );
  const chartBlock = element('div', { class: 'chart-block' });
  const canvas = element('canvas') as HTMLCanvasElement;
  chartBlock.appendChild(canvas);
  overview.appendChild(chartBlock);
  content.appendChild(overview);

  const casesSection = element('div', { class: 'section' });
  casesSection.appendChild(element('h2', { text: 'Cases' }));
  for (const reloadCase of payload.cases) {
    const baselineCase = baselineCases.get(reloadCase.name) ?? null;
    const delta = baselineCase
      ? computeDelta(reloadCase.summary.avg_ms, baselineCase.summary.avg_ms, 'lower')
      : null;
    casesSection.appendChild(renderCaseCard(reloadCase, baselineCase, delta));
  }
  content.appendChild(casesSection);

  return {
    content,
    sidebar: renderSidebar(payload.cases),
    onMount: () => {
      const series = [
        {
          label: 'This run',
          values: payload.cases.map((entry) => entry.summary.avg_ms),
          color: cssColor('--brand-500'),
        },
      ];
      if (baseline) {
        series.push({
          label: 'Baseline',
          values: payload.cases.map(
            (entry) => baselineCases.get(entry.name)?.summary.avg_ms ?? 0,
          ),
          color: 'rgba(160, 160, 160, 0.55)',
        });
      }
      charts.push(
        createBarChart(canvas, {
          categories: payload.cases.map((entry) => humanizeIdentifier(entry.name)),
          series,
          axisLabel: 'Average wall time (ms)',
          stacked: false,
        }),
      );
    },
    onUnmount: () => {
      for (const chart of charts) chart.destroy();
      charts.length = 0;
    },
  };
}

// =============================================================================
// Summary
// =============================================================================

function renderSummaryTiles(
  cases: HotReloadCase[],
  baselineCases: Map<string, HotReloadCase>,
): HTMLElement {
  const row = element('div', { class: 'stat-row' });
  for (const reloadCase of cases) {
    const baselineCase = baselineCases.get(reloadCase.name);
    const delta = baselineCase
      ? computeDelta(reloadCase.summary.avg_ms, baselineCase.summary.avg_ms, 'lower')
      : null;
    const classes = ['stat-tile'];
    if (delta && delta.direction !== 'unchanged') classes.push(delta.direction);
    row.appendChild(
      element(
        'div',
        { class: classes.join(' ') },
        element('div', { class: 'stat-label', text: humanizeIdentifier(reloadCase.name) }),
        element('div', {
          class: 'stat-value',
          text: formatMilliseconds(reloadCase.summary.avg_ms),
        }),
        element('div', {
          class: `stat-delta ${delta ? `change-${delta.direction}` : ''}`,
          text: delta
            ? `${formatRatioPercent(delta.ratio)} · ${delta.label}`
            : `min ${formatMilliseconds(reloadCase.summary.min_ms)} · max ${formatMilliseconds(
                reloadCase.summary.max_ms,
              )}`,
        }),
      ),
    );
  }
  return row;
}

function renderStartupSection(
  sessions: HotReloadSession[],
  baseline: HotReloadMeasurement | null,
): HTMLElement | null {
  const withStartup = sessions.filter((session) => session.startup);
  if (withStartup.length === 0) return null;

  const baselineSessions = baseline
    ? indexBy(baseline.measurement.sessions, (session) => session.name)
    : new Map<string, HotReloadSession>();

  const section = element('div', { class: 'section' });
  section.appendChild(element('h2', { text: 'Host startup' }));
  section.appendChild(
    element('p', {
      class: 'section-hint',
      text: 'Launch to "Entering project loop", plus the host\'s own accounting of what it built.',
    }),
  );

  const table = element('table', { class: 'data-table' });
  table.appendChild(
    tableHead([
      'Session',
      { text: 'Wall time', class: 'numeric' },
      { text: 'Host elapsed', class: 'numeric' },
      { text: 'Builds', class: 'numeric' },
      { text: 'Up-to-date skips', class: 'numeric' },
      { text: 'Δ Baseline', class: 'numeric' },
    ]),
  );
  const body = element('tbody');
  for (const session of withStartup) {
    const startup = session.startup!;
    const baselineStartup = baselineSessions.get(session.name)?.startup ?? null;
    const delta = baselineStartup
      ? computeDelta(startup.wall_ms, baselineStartup.wall_ms, 'lower')
      : null;
    body.appendChild(
      tableRow([
        session.title,
        { text: formatMilliseconds(startup.wall_ms), class: 'numeric' },
        { text: formatMilliseconds(startup.host_elapsed_ms), class: 'numeric' },
        { text: startup.builds !== undefined ? String(startup.builds) : '-', class: 'numeric' },
        {
          text: startup.up_to_date_skips !== undefined ? String(startup.up_to_date_skips) : '-',
          class: 'numeric',
        },
        {
          text: delta
            ? element('span', {
                class: `change-${delta.direction}`,
                text: formatRatioPercent(delta.ratio),
              })
            : '-',
          class: 'numeric',
        },
      ]),
    );
  }
  table.appendChild(body);
  section.appendChild(element('div', { class: 'table-scroll' }, table));
  return section;
}

// =============================================================================
// Case card
// =============================================================================

function renderCaseCard(
  reloadCase: HotReloadCase,
  baselineCase: HotReloadCase | null,
  delta: Delta | null,
): HTMLElement {
  const classes = ['case-card'];
  if (delta && delta.direction !== 'unchanged') classes.push(delta.direction);
  const card = element('div', { class: classes.join(' '), id: `case-${reloadCase.name}` });

  card.appendChild(
    element(
      'div',
      { class: 'case-card-header' },
      element('h3', { text: humanizeIdentifier(reloadCase.name) }),
      element('span', { class: 'case-session', text: reloadCase.session }),
      element('span', {
        class: 'case-headline',
        text: formatMilliseconds(reloadCase.summary.avg_ms),
      }),
    ),
  );
  if (reloadCase.description) {
    card.appendChild(element('p', { class: 'case-description', text: reloadCase.description }));
  }

  // ---- Summary row ----
  const summaryTable = element('table', { class: 'data-table' });
  summaryTable.appendChild(
    tableHead([
      { text: 'Iterations', class: 'numeric' },
      { text: 'Min', class: 'numeric' },
      { text: 'Median', class: 'numeric' },
      { text: 'Average', class: 'numeric' },
      { text: 'Max', class: 'numeric' },
      ...(baselineCase
        ? [
            { text: 'Baseline avg', class: 'numeric' },
            { text: 'Δ Baseline', class: 'numeric' },
          ]
        : []),
    ]),
  );
  const summaryBody = element('tbody');
  summaryBody.appendChild(
    tableRow([
      { text: String(reloadCase.summary.iterations), class: 'numeric' },
      { text: formatMilliseconds(reloadCase.summary.min_ms), class: 'numeric' },
      { text: formatMilliseconds(reloadCase.summary.median_ms), class: 'numeric' },
      { text: formatMilliseconds(reloadCase.summary.avg_ms), class: 'numeric' },
      { text: formatMilliseconds(reloadCase.summary.max_ms), class: 'numeric' },
      ...(baselineCase
        ? [
            { text: formatMilliseconds(baselineCase.summary.avg_ms), class: 'numeric' },
            {
              text: delta
                ? element('span', {
                    class: `change-${delta.direction}`,
                    text: `${formatRatioPercent(delta.ratio)} · ${delta.label}`,
                  })
                : '-',
              class: 'numeric',
            },
          ]
        : []),
    ]),
  );
  summaryTable.appendChild(summaryBody);
  card.appendChild(element('div', { class: 'table-scroll' }, summaryTable));

  // ---- Phase breakdown ----
  const phases = reloadCase.summary.phase_averages;
  if (phases && Object.keys(phases).length > 0) {
    card.appendChild(renderPhaseBar(phases, reloadCase.summary.avg_ms));
  } else {
    card.appendChild(
      element('p', {
        class: 'no-data',
        text: 'No host phase analytics for this category - wall time only.',
      }),
    );
  }

  // ---- Per-iteration detail ----
  card.appendChild(renderIterationTable(reloadCase));
  return card;
}

/**
 * Draws the phase split as a proportional bar plus a legend.
 *
 * The measured phases rarely sum to the wall time - detection latency and
 * process scheduling live outside them - so the remainder is shown as its own
 * "unaccounted" segment instead of being silently absorbed.
 */
function renderPhaseBar(phases: PhaseValues, averageWallMs: number): HTMLElement {
  const container = element('div');
  const bar = element('div', { class: 'phase-bar' });
  const legend = element('div', { class: 'phase-legend' });

  const total = Math.max(averageWallMs, 0.001);
  let accounted = 0;
  for (const phase of PHASES) {
    const value = phases[phase.key];
    if (value === undefined) continue;
    accounted += value;
    const color = cssColor(phase.variable);
    const segment = element('div', { class: 'phase-segment' });
    segment.style.width = `${(value / total) * 100}%`;
    segment.style.background = color;
    segment.title = `${phase.label}: ${formatMilliseconds(value)}`;
    bar.appendChild(segment);

    const swatch = element('span', { class: 'legend-swatch' });
    swatch.style.background = color;
    legend.appendChild(
      element(
        'span',
        { class: 'legend-entry' },
        swatch,
        element('span', { text: `${phase.label} ${formatMilliseconds(value)}` }),
      ),
    );
  }

  const remainder = total - accounted;
  if (remainder > 0.5) {
    const segment = element('div', { class: 'phase-segment' });
    segment.style.width = `${(remainder / total) * 100}%`;
    segment.style.background = 'rgba(255, 255, 255, 0.08)';
    segment.title = `unaccounted: ${formatMilliseconds(remainder)}`;
    bar.appendChild(segment);

    const swatch = element('span', { class: 'legend-swatch' });
    swatch.style.background = 'rgba(255, 255, 255, 0.12)';
    legend.appendChild(
      element(
        'span',
        { class: 'legend-entry' },
        swatch,
        element('span', {
          text: `watcher + scheduling ${formatMilliseconds(remainder)}`,
        }),
      ),
    );
  }

  container.appendChild(bar);
  container.appendChild(legend);
  return container;
}

function renderIterationTable(reloadCase: HotReloadCase): HTMLElement {
  const hasPhases = reloadCase.iterations.some((iteration) => iteration.build_ms !== undefined);
  const table = element('table', { class: 'data-table' });
  const headers: (string | { text: string; class?: string })[] = [
    { text: '#', class: 'rank' },
    { text: 'Wall', class: 'numeric' },
  ];
  if (hasPhases) {
    for (const phase of PHASES) headers.push({ text: phase.label, class: 'numeric' });
  }
  table.appendChild(tableHead(headers));

  const body = element('tbody');
  for (const iteration of reloadCase.iterations) {
    const cells: Parameters<typeof tableRow>[0] = [
      { text: String(iteration.index), class: 'rank' },
      { text: formatMilliseconds(iteration.wall_ms), class: 'numeric' },
    ];
    if (hasPhases) {
      for (const phase of PHASES) {
        cells.push({ text: formatMilliseconds(iteration[phase.key]), class: 'numeric' });
      }
    }
    body.appendChild(tableRow(cells));
  }
  table.appendChild(body);

  const container = element('div');
  container.appendChild(element('div', { class: 'table-scroll' }, table));

  // The crates cargo rebuilt explain a slow build phase, so the newest
  // iteration's list is shown directly under the table.
  const lastWithCrates = [...reloadCase.iterations]
    .reverse()
    .find((iteration) => iteration.rebuilt_crates?.length);
  if (lastWithCrates?.rebuilt_crates) {
    const crateList = element('div', { class: 'crate-list' });
    for (const crate of lastWithCrates.rebuilt_crates) {
      crateList.appendChild(element('span', { class: 'crate-pill', text: crate }));
    }
    container.appendChild(
      element('div', {
        class: 'sidebar-section-label',
        text: `Crates rebuilt by cargo (iteration ${lastWithCrates.index})`,
      }),
    );
    container.appendChild(crateList);
  }
  return container;
}

// =============================================================================
// Sidebar
// =============================================================================

function renderSidebar(cases: HotReloadCase[]): HTMLElement {
  const container = element('div');
  container.appendChild(element('div', { class: 'sidebar-section-label', text: 'Cases' }));
  const group = element('div', { class: 'toc-group' });
  for (const reloadCase of cases) {
    const item = element(
      'a',
      { class: 'toc-item' },
      element('span', { class: 'direction-dot unchanged' }),
      element('span', { text: humanizeIdentifier(reloadCase.name) }),
    );
    scrollToOnClick(item, `case-${reloadCase.name}`);
    group.appendChild(item);
  }
  container.appendChild(group);
  return container;
}

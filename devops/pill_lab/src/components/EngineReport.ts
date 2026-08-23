// DESCRIPTION: The Engine Performance view - the direct successor to the HTML
//   `gen_bench_report.py` used to generate. It keeps that report's structure
//   (summary, regression banner, leaderboard, grouped benchmark cards with a
//   scatter/histogram chart and a 95% CI statistics table) and adds Pill Lab's
//   baseline comparison as a second, independent change column.
//
//   Two change axes exist and are labelled distinctly so they cannot be
//   confused:
//     "Δ Criterion" - Criterion's own comparison with its immediately
//                     preceding run, stored inside target/criterion.
//     "Δ Baseline"  - this measurement's mean against the mean of whichever
//                     stored run the user picked as the baseline.
//
//   Charts are created lazily as cards scroll into view: a full benchmark set
//   is 45+ canvases, and building them all up front would stall the page.
//
// --- SCRIPT ---

import type { Chart } from 'chart.js';
import { createHistogramChart, createScatterChart, sparklineSvg } from '../lib/charts';
import { computeDelta, indexBy, tallyDirections, type Delta } from '../lib/compare';
import { appendChildren, element, scrollToOnClick, tableHead, tableRow } from '../lib/dom';
import {
  formatNanoseconds,
  formatRatioPercent,
  formatThroughput,
  humanizeIdentifier,
  splitBenchmarkId,
} from '../lib/format';
import type {
  BenchmarkEntry,
  ChangeDirection,
  EngineMeasurement,
  EstimateBlock,
} from '../types/measurement';
import type { ReportView } from './ReportView';

/** Renders a change value with the CSS class matching its direction. */
function changeCell(text: string, direction: ChangeDirection): HTMLElement {
  return element('span', { class: `change-${direction}`, text });
}

/** Builds one `label | lower | estimate | upper` statistics row. */
function estimateRow(label: string, block: EstimateBlock | undefined): HTMLElement | null {
  if (!block) return null;
  return tableRow([
    label,
    { text: formatNanoseconds(block.lower), class: 'ci-bound' },
    formatNanoseconds(block.point),
    { text: formatNanoseconds(block.upper), class: 'ci-bound' },
  ]);
}

/** Builds a single-value row spanning the estimate columns. */
function spanRow(label: string, value: string): HTMLElement {
  return tableRow([label, { text: value, colSpan: 3 }]);
}

/** Chooses the card's left-rail direction: baseline delta wins when present. */
function cardDirection(benchmark: BenchmarkEntry, delta: Delta | null): ChangeDirection {
  if (delta) return delta.direction;
  return benchmark.change?.direction ?? 'unchanged';
}

export function renderEngineReport(
  measurement: EngineMeasurement,
  baseline: EngineMeasurement | null,
): ReportView {
  const benchmarks = measurement.measurement.benchmarks;
  const baselineIndex = baseline
    ? indexBy(baseline.measurement.benchmarks, (entry) => entry.id)
    : new Map<string, BenchmarkEntry>();

  // One delta per benchmark, computed once and reused by every view below.
  const deltas = new Map<string, Delta | null>();
  for (const benchmark of benchmarks) {
    const baselineEntry = baselineIndex.get(benchmark.id);
    deltas.set(
      benchmark.id,
      baselineEntry ? computeDelta(benchmark.mean_ns, baselineEntry.mean_ns, 'lower') : null,
    );
  }

  const content = element('div');
  const chartRegistry = new Map<string, Chart>();
  const pendingCharts: (() => void)[] = [];

  content.appendChild(renderSummary(measurement, baseline, deltas));
  const banner = renderChangeBanner(measurement, baseline, deltas);
  if (banner) content.appendChild(banner);

  content.appendChild(renderTopBar(content));
  content.appendChild(renderLeaderboard(benchmarks, deltas, Boolean(baseline)));

  for (const group of measurement.measurement.groups) {
    const groupBenchmarks = group.benchmark_ids
      .map((id) => benchmarks.find((entry) => entry.id === id))
      .filter((entry): entry is BenchmarkEntry => Boolean(entry));
    if (groupBenchmarks.length === 0) continue;

    const section = element('div', { class: 'benchmark-group' });
    section.appendChild(element('h2', { text: humanizeIdentifier(group.label) }));
    section.appendChild(element('p', { class: 'group-hint', text: group.label }));
    section.appendChild(element('hr'));
    for (const benchmark of groupBenchmarks) {
      section.appendChild(
        renderBenchmarkCard(
          benchmark,
          deltas.get(benchmark.id) ?? null,
          baselineIndex.get(benchmark.id) ?? null,
          Boolean(baseline),
          chartRegistry,
          pendingCharts,
        ),
      );
    }
    content.appendChild(section);
  }

  content.appendChild(
    element('div', {
      id: 'footer',
      text:
        `Criterion data from ${measurement.measurement.criterion_directory} · ` +
        `profile ${measurement.measurement.profile}` +
        (measurement.measurement.quick ? ' · --quick (reduced precision)' : ''),
    }),
  );

  return {
    content,
    sidebar: renderSidebar(measurement, deltas, content),
    onMount: () => mountChartsLazily(content, pendingCharts),
    onUnmount: () => {
      for (const chart of chartRegistry.values()) chart.destroy();
      chartRegistry.clear();
    },
  };
}

// =============================================================================
// Summary and banners
// =============================================================================

function renderSummary(
  measurement: EngineMeasurement,
  baseline: EngineMeasurement | null,
  deltas: Map<string, Delta | null>,
): HTMLElement {
  const benchmarks = measurement.measurement.benchmarks;
  const row = element('div', { class: 'stat-row' });

  row.appendChild(statTile('Benchmarks', String(benchmarks.length), ''));

  const criterionTally = benchmarks.reduce(
    (tally, benchmark) => {
      const direction = benchmark.change?.direction;
      if (direction) tally[direction] += 1;
      return tally;
    },
    { improved: 0, regressed: 0, unchanged: 0 } as Record<ChangeDirection, number>,
  );
  row.appendChild(
    statTile(
      'Δ Criterion',
      `${criterionTally.regressed} regressed`,
      `${criterionTally.improved} improved · ${criterionTally.unchanged} flat`,
      criterionTally.regressed > 0 ? 'regressed' : 'improved',
    ),
  );

  if (baseline) {
    const baselineTally = tallyDirections([...deltas.values()]);
    const compared = baselineTally.improved + baselineTally.regressed + baselineTally.unchanged;
    row.appendChild(
      statTile(
        'Δ Baseline',
        `${baselineTally.regressed} regressed`,
        `${baselineTally.improved} improved · ${compared} of ${benchmarks.length} matched`,
        baselineTally.regressed > 0 ? 'regressed' : 'improved',
      ),
    );
  }

  const slowest = [...benchmarks].sort((left, right) => right.mean_ns - left.mean_ns)[0];
  if (slowest) {
    row.appendChild(statTile('Slowest', formatNanoseconds(slowest.mean_ns), slowest.id));
  }

  return row;
}

function statTile(
  label: string,
  value: string,
  detail: string,
  direction?: ChangeDirection,
): HTMLElement {
  const classes = ['stat-tile'];
  if (direction && direction !== 'unchanged') classes.push(direction);
  return element(
    'div',
    { class: classes.join(' ') },
    element('div', { class: 'stat-label', text: label }),
    element('div', { class: 'stat-value', text: value }),
    detail ? element('div', { class: 'stat-delta', text: detail }) : null,
  );
}

/** Lists the worst regressions so they are visible without scrolling. */
function renderChangeBanner(
  measurement: EngineMeasurement,
  baseline: EngineMeasurement | null,
  deltas: Map<string, Delta | null>,
): HTMLElement | null {
  const source = baseline ? 'the selected baseline' : 'the previous Criterion run';
  const regressions: { id: string; text: string }[] = [];

  for (const benchmark of measurement.measurement.benchmarks) {
    if (baseline) {
      const delta = deltas.get(benchmark.id);
      if (delta && delta.direction === 'regressed') {
        regressions.push({ id: benchmark.id, text: delta.label });
      }
    } else if (benchmark.change?.direction === 'regressed') {
      regressions.push({ id: benchmark.id, text: formatRatioPercent(benchmark.change.percent) });
    }
  }
  if (regressions.length === 0) return null;

  // Show the five worst; the leaderboard carries the complete picture.
  const shown = regressions.slice(0, 5);
  const banner = element(
    'div',
    { class: 'banner' },
    element('strong', {
      text: `${regressions.length} benchmark${regressions.length === 1 ? '' : 's'} regressed against ${source}:`,
    }),
  );
  const list = element('ul');
  for (const regression of shown) {
    list.appendChild(element('li', { text: `${regression.id}: ${regression.text}` }));
  }
  if (regressions.length > shown.length) {
    list.appendChild(element('li', { text: `… and ${regressions.length - shown.length} more` }));
  }
  banner.appendChild(list);
  return banner;
}

function renderTopBar(content: HTMLElement): HTMLElement {
  const bar = element('div', { class: 'top-bar' });
  const collapseAll = element('button', { type: 'button', text: 'Collapse all' });
  collapseAll.addEventListener('click', () => {
    content.querySelectorAll('.benchmark').forEach((card) => card.classList.add('collapsed'));
  });
  const expandAll = element('button', { type: 'button', text: 'Expand all' });
  expandAll.addEventListener('click', () => {
    content.querySelectorAll('.benchmark').forEach((card) => card.classList.remove('collapsed'));
  });
  bar.appendChild(collapseAll);
  bar.appendChild(expandAll);
  return bar;
}

// =============================================================================
// Leaderboard
// =============================================================================

function renderLeaderboard(
  benchmarks: BenchmarkEntry[],
  deltas: Map<string, Delta | null>,
  hasBaseline: boolean,
): HTMLElement {
  const section = element('div', { class: 'section' });
  section.appendChild(element('h2', { text: 'Leaderboard (slowest → fastest)' }));
  section.appendChild(
    element('p', {
      class: 'section-hint',
      text: 'Mean time per iteration. Click a name to jump to its card.',
    }),
  );

  const table = element('table', { class: 'data-table' });
  const headers: (string | { text: string; class?: string })[] = [
    { text: '#', class: 'rank' },
    'Benchmark',
    { text: 'Mean time', class: 'numeric' },
    { text: 'Throughput', class: 'numeric' },
    { text: 'Δ Criterion', class: 'numeric' },
  ];
  if (hasBaseline) headers.push({ text: 'Δ Baseline', class: 'numeric' });
  headers.push({ text: 'Iters', class: 'numeric' });
  table.appendChild(tableHead(headers));

  const body = element('tbody');
  const ordered = [...benchmarks].sort((left, right) => right.mean_ns - left.mean_ns);
  ordered.forEach((benchmark, index) => {
    const { name, parameter } = splitBenchmarkId(benchmark.id);
    const link = element('a', { text: name });
    scrollToOnClick(link, `bench-${cssId(benchmark.id)}`);
    const nameCell = element(
      'span',
      {},
      element('span', {
        class: `direction-dot ${benchmark.change?.direction ?? 'unchanged'}`,
      }),
      ' ',
      link,
      parameter ? element('span', { class: 'param-badge', text: `x${parameter}` }) : null,
    );

    const criterionChange = benchmark.change;
    const delta = deltas.get(benchmark.id) ?? null;
    const cells: Parameters<typeof tableRow>[0] = [
      { text: String(index + 1), class: 'rank' },
      nameCell,
      { text: formatNanoseconds(benchmark.mean_ns), class: 'numeric' },
      { text: formatThroughput(benchmark.throughput), class: 'numeric' },
      {
        text: criterionChange
          ? changeCell(formatRatioPercent(criterionChange.percent), criterionChange.direction)
          : '-',
        class: 'numeric',
      },
    ];
    if (hasBaseline) {
      cells.push({
        text: delta ? changeCell(formatRatioPercent(delta.ratio), delta.direction) : '-',
        class: 'numeric',
      });
    }
    cells.push({ text: String(benchmark.iteration_count), class: 'numeric' });
    body.appendChild(tableRow(cells));
  });
  table.appendChild(body);

  section.appendChild(element('div', { class: 'table-scroll' }, table));
  return section;
}

// =============================================================================
// Benchmark card
// =============================================================================

/** Makes a benchmark id safe to use inside an element id / fragment link. */
function cssId(benchmarkId: string): string {
  return benchmarkId.replace(/[^a-zA-Z0-9_-]/g, '_');
}

function renderBenchmarkCard(
  benchmark: BenchmarkEntry,
  delta: Delta | null,
  baselineEntry: BenchmarkEntry | null,
  hasBaseline: boolean,
  chartRegistry: Map<string, Chart>,
  pendingCharts: (() => void)[],
): HTMLElement {
  const direction = cardDirection(benchmark, delta);
  const card = element('section', {
    class: `benchmark ${direction}`,
    id: `bench-${cssId(benchmark.id)}`,
    'data-name': benchmark.id,
  });

  const { name, parameter } = splitBenchmarkId(benchmark.id);
  const header = element(
    'div',
    { class: 'benchmark-header' },
    element('span', { class: 'collapse-icon', text: '▾' }),
    element(
      'h3',
      {},
      name,
      parameter ? element('span', { class: 'param-badge', text: `x${parameter}` }) : null,
    ),
    delta
      ? element('span', {
          class: `header-delta change-${delta.direction}`,
          text: delta.label,
        })
      : null,
    element('span', { class: 'header-metric', text: formatNanoseconds(benchmark.mean_ns) }),
  );
  header.addEventListener('click', () => card.classList.toggle('collapsed'));
  card.appendChild(header);

  const body = element('div', { class: 'benchmark-body' });
  const grid = element('div', { class: 'benchmark-grid' });

  // ---- Chart ----
  const chartContainer = element('div', { class: 'chart-container' });
  const toolbar = element('div', { class: 'chart-toolbar' });
  const canvas = element('canvas') as HTMLCanvasElement;
  const scatterButton = element('button', { type: 'button', class: 'active', text: 'Scatter' });
  const histogramButton = element('button', { type: 'button', text: 'Histogram' });

  const buildScatter = (): Chart | null => {
    if (benchmark.samples_us.length === 0) return null;
    const mean = benchmark.estimates.mean;
    return createScatterChart(canvas, {
      label: `${benchmark.id} (µs)`,
      samplesMicros: benchmark.samples_us,
      outlierFlags: benchmark.outlier_flags,
      // The baseline overlay prefers the selected Pill Lab run and falls back
      // to Criterion's own stored previous run.
      baselineSamplesMicros: baselineEntry?.samples_us ?? benchmark.base_samples_us,
      baselineLabel: baselineEntry ? 'Baseline run (µs)' : 'Previous Criterion run (µs)',
      confidenceLower: (mean?.lower ?? 0) / 1000,
      confidenceUpper: (mean?.upper ?? 0) / 1000,
      meanMicros: benchmark.mean_ns / 1000,
    });
  };

  const swapChart = (factory: () => Chart | null, active: HTMLElement): void => {
    chartRegistry.get(benchmark.id)?.destroy();
    const created = factory();
    if (created) chartRegistry.set(benchmark.id, created);
    else chartRegistry.delete(benchmark.id);
    toolbar.querySelectorAll('button').forEach((button) => button.classList.remove('active'));
    active.classList.add('active');
  };

  scatterButton.addEventListener('click', () => swapChart(buildScatter, scatterButton));
  histogramButton.addEventListener('click', () =>
    swapChart(
      () => createHistogramChart(canvas, benchmark.samples_us, benchmark.mean_ns / 1000),
      histogramButton,
    ),
  );

  toolbar.appendChild(scatterButton);
  toolbar.appendChild(histogramButton);
  chartContainer.appendChild(toolbar);
  chartContainer.appendChild(canvas);
  grid.appendChild(chartContainer);

  // Deferred until the card is near the viewport (see mountChartsLazily).
  pendingCharts.push(() => {
    if (chartRegistry.has(benchmark.id)) return;
    const created = buildScatter();
    if (created) chartRegistry.set(benchmark.id, created);
  });
  card.setAttribute('data-chart-pending', String(pendingCharts.length - 1));

  // ---- Statistics ----
  grid.appendChild(renderStatsTable(benchmark, delta, baselineEntry, hasBaseline, canvas));

  body.appendChild(grid);
  card.appendChild(body);
  return card;
}

function renderStatsTable(
  benchmark: BenchmarkEntry,
  delta: Delta | null,
  baselineEntry: BenchmarkEntry | null,
  hasBaseline: boolean,
  canvas: HTMLCanvasElement,
): HTMLElement {
  const container = element('div', { class: 'stats-table' });
  container.appendChild(element('h4', { text: 'Statistics (95% CI)' }));

  const table = element('table');
  table.appendChild(
    tableHead(['', { text: 'Lower', class: 'ci-bound' }, 'Estimate', { text: 'Upper', class: 'ci-bound' }]),
  );
  const body = element('tbody');
  appendChildren(body, [
    estimateRow('Mean', benchmark.estimates.mean),
    estimateRow('Median', benchmark.estimates.median),
    estimateRow('Std. Dev.', benchmark.estimates.std_dev),
    estimateRow('Slope', benchmark.estimates.slope),
  ]);
  if (benchmark.iteration_count) body.appendChild(spanRow('Iterations', String(benchmark.iteration_count)));
  if (benchmark.min_ns !== null) body.appendChild(spanRow('Min', formatNanoseconds(benchmark.min_ns)));
  if (benchmark.max_ns !== null) body.appendChild(spanRow('Max', formatNanoseconds(benchmark.max_ns)));
  if (benchmark.outlier_count) body.appendChild(spanRow('Outliers', String(benchmark.outlier_count)));
  if (benchmark.throughput !== null) {
    body.appendChild(
      spanRow('Throughput', `${formatThroughput(benchmark.throughput)} (${benchmark.throughput_unit})`),
    );
  }
  table.appendChild(body);
  container.appendChild(table);

  // ---- Criterion's own change ----
  if (benchmark.change) {
    container.appendChild(element('h4', { class: 'spaced', text: 'Δ vs previous Criterion run' }));
    const changeTable = element('table');
    changeTable.appendChild(
      tableHead(['', { text: 'Lower', class: 'ci-bound' }, 'Estimate', { text: 'Upper', class: 'ci-bound' }]),
    );
    const changeBody = element('tbody');
    changeBody.appendChild(
      tableRow([
        'Change',
        { text: formatRatioPercent(benchmark.change.lower), class: 'ci-bound' },
        {
          text: changeCell(formatRatioPercent(benchmark.change.percent), benchmark.change.direction),
        },
        { text: formatRatioPercent(benchmark.change.upper), class: 'ci-bound' },
      ]),
    );
    changeTable.appendChild(changeBody);
    container.appendChild(changeTable);
  }

  // ---- Pill Lab baseline comparison ----
  if (hasBaseline) {
    container.appendChild(element('h4', { class: 'spaced', text: 'Δ vs selected baseline' }));
    const baselineTable = element('table');
    const baselineBody = element('tbody');
    if (delta && baselineEntry) {
      baselineBody.appendChild(
        tableRow(['Baseline mean', { text: formatNanoseconds(baselineEntry.mean_ns), colSpan: 3 }]),
      );
      baselineBody.appendChild(
        tableRow([
          'Change',
          {
            text: changeCell(
              `${formatRatioPercent(delta.ratio)} · ${delta.label}`,
              delta.direction,
            ),
            colSpan: 3,
          },
        ]),
      );
    } else {
      baselineBody.appendChild(
        tableRow(['Change', { text: 'not present in the baseline run', colSpan: 3 }]),
      );
    }
    baselineTable.appendChild(baselineBody);
    container.appendChild(baselineTable);
  }

  container.appendChild(renderActionButtons(benchmark, canvas));
  return container;
}

function renderActionButtons(benchmark: BenchmarkEntry, canvas: HTMLCanvasElement): HTMLElement {
  const buttons = element('div', { class: 'action-buttons' });

  const copyButton = element('button', { type: 'button', text: 'Copy MD' });
  copyButton.title = 'Copy this benchmark as a Markdown table row';
  copyButton.addEventListener('click', () => {
    const row =
      `| ${benchmark.id} | ${(benchmark.mean_ns / 1000).toFixed(2)} µs ` +
      `| ${(benchmark.median_ns / 1000).toFixed(2)} µs ` +
      `| ${(benchmark.std_dev_ns / 1000).toFixed(2)} µs |`;
    void navigator.clipboard.writeText(row).then(
      () => {
        copyButton.textContent = 'Copied!';
        window.setTimeout(() => {
          copyButton.textContent = 'Copy MD';
        }, 1200);
      },
      () => {
        copyButton.textContent = 'Copy failed';
      },
    );
  });

  const pngButton = element('button', { type: 'button', text: 'PNG' });
  pngButton.title = 'Download the chart as a PNG image';
  pngButton.addEventListener('click', () => {
    const link = document.createElement('a');
    link.download = `${benchmark.id.replace(/[^a-zA-Z0-9]/g, '_')}.png`;
    link.href = canvas.toDataURL('image/png');
    link.click();
  });

  buttons.appendChild(copyButton);
  buttons.appendChild(pngButton);
  return buttons;
}

// =============================================================================
// Lazy chart mounting
// =============================================================================

/**
 * Creates each card's chart the first time it approaches the viewport.
 *
 * A full run renders dozens of canvases; building them eagerly costs seconds
 * of main-thread time for charts most readers never scroll to.
 */
function mountChartsLazily(content: HTMLElement, pendingCharts: (() => void)[]): void {
  const cards = content.querySelectorAll<HTMLElement>('.benchmark[data-chart-pending]');
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        const target = entry.target as HTMLElement;
        const index = Number(target.getAttribute('data-chart-pending'));
        pendingCharts[index]?.();
        target.removeAttribute('data-chart-pending');
        observer.unobserve(target);
      }
    },
    { rootMargin: '400px 0px' },
  );
  cards.forEach((card) => observer.observe(card));
}

// =============================================================================
// Sidebar: filter + benchmark table of contents
// =============================================================================

function renderSidebar(
  measurement: EngineMeasurement,
  deltas: Map<string, Delta | null>,
  content: HTMLElement,
): HTMLElement {
  const container = element('div');
  container.appendChild(
    element('div', {
      class: 'sidebar-section-label',
      text: `${measurement.measurement.benchmarks.length} benchmarks`,
    }),
  );

  const search = element('input', {
    class: 'search-input',
    type: 'text',
    placeholder: 'Filter benchmarks...',
  }) as HTMLInputElement;
  container.appendChild(search);

  const list = element('div');
  for (const group of measurement.measurement.groups) {
    const groupElement = element('div', { class: 'toc-group' });
    groupElement.appendChild(
      element('div', { class: 'toc-group-header', text: humanizeIdentifier(group.label) }),
    );
    for (const id of group.benchmark_ids) {
      const benchmark = measurement.measurement.benchmarks.find((entry) => entry.id === id);
      if (!benchmark) continue;
      const delta = deltas.get(id) ?? null;
      const direction = cardDirection(benchmark, delta);
      const item = element(
        'a',
        { class: 'toc-item', 'data-name': id },
        element('span', { class: `direction-dot ${direction}`, title: direction }),
      );
      scrollToOnClick(item, `bench-${cssId(id)}`);
      const sparkline = sparklineSvg(benchmark.samples_us);
      if (sparkline) item.appendChild(sparkline);
      item.appendChild(element('span', { text: benchmark.parameter || benchmark.id }));
      groupElement.appendChild(item);
    }
    list.appendChild(groupElement);
  }
  container.appendChild(list);

  // Filtering hides both the sidebar entries and the cards, so the main
  // column and the table of contents always agree on what is visible.
  search.addEventListener('input', () => {
    const query = search.value.trim().toLowerCase();
    content.querySelectorAll<HTMLElement>('.benchmark').forEach((card) => {
      const name = (card.getAttribute('data-name') ?? '').toLowerCase();
      card.classList.toggle('hidden', query !== '' && !name.includes(query));
    });
    list.querySelectorAll<HTMLElement>('.toc-item').forEach((item) => {
      const name = (item.getAttribute('data-name') ?? '').toLowerCase();
      item.classList.toggle('hidden', query !== '' && !name.includes(query));
    });
  });

  return container;
}

// DESCRIPTION: The Cold Start view. Build cases and startup cases are kept in
//   separate sections and never averaged together: a clean build and an
//   incremental build measure different things, and calling an incremental
//   build "cold" would be wrong.
//
//   Each build case carries Cargo's own `--timings` data, rendered as a
//   slowest-units table plus the effective build parallelism, rather than by
//   embedding Cargo's HTML report.
//
// --- SCRIPT ---

import type { Chart } from 'chart.js';
import { createBarChart } from '../lib/charts';
import { computeDelta, indexBy, type Delta } from '../lib/compare';
import { collapsiblePanel } from './EnvironmentInfo';
import { element, scrollToOnClick, tableHead, tableRow } from '../lib/dom';
import {
  formatMilliseconds,
  formatRatioPercent,
  formatSeconds,
  humanizeIdentifier,
} from '../lib/format';
import type { ColdStartCase, ColdStartMeasurement } from '../types/measurement';
import type { ReportView } from './ReportView';

/** Reads a CSS custom property so charts share the page palette. */
function cssColor(variable: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(variable).trim() || '#888';
}

export function renderColdStartReport(
  measurement: ColdStartMeasurement,
  baseline: ColdStartMeasurement | null,
): ReportView {
  const payload = measurement.measurement;
  const baselineCases = baseline
    ? indexBy(baseline.measurement.cases, (entry) => entry.name)
    : new Map<string, ColdStartCase>();

  const buildCases = payload.cases.filter((entry) => entry.kind === 'build');
  const startupCases = payload.cases.filter((entry) => entry.kind === 'startup');

  const content = element('div');
  const charts: Chart[] = [];

  content.appendChild(renderSummaryTiles(payload.cases, baselineCases));
  content.appendChild(renderScopePanel(measurement));

  const chartCanvas = element('canvas') as HTMLCanvasElement;
  if (payload.cases.length > 0) {
    const section = element('div', { class: 'section' });
    section.appendChild(element('h2', { text: 'Case durations' }));
    section.appendChild(
      element('p', {
        class: 'section-hint',
        text: 'Clean, incremental and startup cases side by side. Lower is better.',
      }),
    );
    section.appendChild(element('div', { class: 'chart-block' }, chartCanvas));
    content.appendChild(section);
  }

  if (buildCases.length > 0) {
    content.appendChild(
      renderCaseSection(
        'Compilation',
        'Clean cases follow a targeted cargo clean; incremental cases follow an mtime bump of ' +
          payload.incremental_trigger + '.',
        buildCases,
        baselineCases,
      ),
    );
  }
  if (startupCases.length > 0) {
    content.appendChild(
      renderCaseSection(
        'Startup',
        'Host launch to a usable engine, and the pill_engine smoke binary end to end.',
        startupCases,
        baselineCases,
      ),
    );
  }

  return {
    content,
    sidebar: renderSidebar(buildCases, startupCases),
    onMount: () => {
      if (payload.cases.length === 0) return;
      const series = [
        {
          label: 'This run',
          values: payload.cases.map((entry) => entry.duration_ms / 1000),
          color: cssColor('--brand-500'),
        },
      ];
      if (baseline) {
        series.push({
          label: 'Baseline',
          values: payload.cases.map(
            (entry) => (baselineCases.get(entry.name)?.duration_ms ?? 0) / 1000,
          ),
          color: 'rgba(160, 160, 160, 0.55)',
        });
      }
      charts.push(
        createBarChart(chartCanvas, {
          categories: payload.cases.map((entry) => humanizeIdentifier(entry.name)),
          series,
          axisLabel: 'Duration (s)',
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
  cases: ColdStartCase[],
  baselineCases: Map<string, ColdStartCase>,
): HTMLElement {
  const row = element('div', { class: 'stat-row' });
  // These four answer "how long until I can run my change?" at a glance.
  const highlights = ['clean_build', 'incremental_build', 'startup_cold', 'startup_warm'];
  const shown = cases.filter((entry) => highlights.includes(entry.name));
  for (const coldCase of shown.length > 0 ? shown : cases.slice(0, 4)) {
    const baselineCase = baselineCases.get(coldCase.name);
    const delta = baselineCase
      ? computeDelta(coldCase.duration_ms, baselineCase.duration_ms, 'lower')
      : null;
    const classes = ['stat-tile'];
    if (delta && delta.direction !== 'unchanged') classes.push(delta.direction);
    row.appendChild(
      element(
        'div',
        { class: classes.join(' ') },
        element('div', { class: 'stat-label', text: humanizeIdentifier(coldCase.name) }),
        element('div', { class: 'stat-value', text: formatMilliseconds(coldCase.duration_ms) }),
        element('div', {
          class: `stat-delta ${delta ? `change-${delta.direction}` : ''}`,
          text: delta
            ? `${formatRatioPercent(delta.ratio)} · ${delta.label}`
            : coldCase.cargo_timings
              ? `${coldCase.cargo_timings.unit_count} units compiled`
              : '',
        }),
      ),
    );
  }
  return row;
}

/** States exactly what was cleaned, so a "cold" number can be trusted. */
function renderScopePanel(measurement: ColdStartMeasurement): HTMLElement {
  const payload = measurement.measurement;
  const body = element('div');

  const scopeText =
    payload.clean_scope === 'workspace'
      ? 'The entire target directory was removed, so every third-party dependency was recompiled.'
      : payload.clean_scope === 'packages'
        ? 'Only this workspace\'s own packages were removed from the target directory. ' +
          'Third-party dependency artifacts stayed compiled, so a "clean" case here means ' +
          '"rebuild the engine from scratch", not "rebuild the world".'
        : 'Nothing was cleaned: only incremental and startup cases were measured.';
  body.appendChild(element('p', { class: 'case-description', text: scopeText }));

  if (payload.cleaned_packages.length > 0) {
    const list = element('div', { class: 'crate-list' });
    for (const packageName of payload.cleaned_packages) {
      list.appendChild(element('span', { class: 'crate-pill', text: packageName }));
    }
    body.appendChild(list);
  }

  const hint = `${payload.clean_scope} scope · ${payload.cleaned_packages.length} package(s)`;
  return collapsiblePanel('Clean scope', hint, body);
}

// =============================================================================
// Case sections
// =============================================================================

function renderCaseSection(
  title: string,
  hint: string,
  cases: ColdStartCase[],
  baselineCases: Map<string, ColdStartCase>,
): HTMLElement {
  const section = element('div', { class: 'section' });
  section.appendChild(element('h2', { text: title }));
  section.appendChild(element('p', { class: 'section-hint', text: hint }));
  for (const coldCase of cases) {
    const baselineCase = baselineCases.get(coldCase.name) ?? null;
    const delta = baselineCase
      ? computeDelta(coldCase.duration_ms, baselineCase.duration_ms, 'lower')
      : null;
    section.appendChild(renderCaseCard(coldCase, baselineCase, delta));
  }
  return section;
}

function renderCaseCard(
  coldCase: ColdStartCase,
  baselineCase: ColdStartCase | null,
  delta: Delta | null,
): HTMLElement {
  const classes = ['case-card'];
  if (delta && delta.direction !== 'unchanged') classes.push(delta.direction);
  const card = element('div', { class: classes.join(' '), id: `case-${coldCase.name}` });

  card.appendChild(
    element(
      'div',
      { class: 'case-card-header' },
      element('h3', { text: humanizeIdentifier(coldCase.name) }),
      element('span', { class: 'case-session', text: coldCase.kind }),
      element('span', {
        class: 'case-headline',
        text: formatMilliseconds(coldCase.duration_ms),
      }),
    ),
  );
  if (coldCase.description) {
    card.appendChild(element('p', { class: 'case-description', text: coldCase.description }));
  }

  const facts = element('table', { class: 'data-table' });
  const factBody = element('tbody');
  const addFact = (label: string, value: string | HTMLElement): void => {
    factBody.appendChild(tableRow([label, { text: value, class: 'numeric' }]));
  };

  addFact('Command', coldCase.command.join(' '));
  if (coldCase.repetitions) {
    addFact(
      'Repetitions',
      `${coldCase.repetitions} runs · min ${formatMilliseconds(coldCase.min_ms)} · ` +
        `avg ${formatMilliseconds(coldCase.avg_ms)} · max ${formatMilliseconds(coldCase.max_ms)}`,
    );
  }
  if (coldCase.host_elapsed_ms !== undefined) {
    addFact('Host self-reported elapsed', formatMilliseconds(coldCase.host_elapsed_ms));
  }
  if (coldCase.builds !== undefined) {
    addFact('Modules built', String(coldCase.builds));
  }
  if (coldCase.up_to_date_skips !== undefined) {
    addFact('Up-to-date skips', String(coldCase.up_to_date_skips));
  }
  const timings = coldCase.cargo_timings;
  if (timings) {
    addFact('Cargo total', formatSeconds(timings.total_seconds));
    addFact('Units compiled', String(timings.unit_count));
    if (timings.parallelism !== null) {
      addFact('Effective parallelism', `${timings.parallelism.toFixed(2)}x`);
    }
  }
  if (baselineCase && delta) {
    addFact(
      'Baseline',
      element(
        'span',
        {},
        `${formatMilliseconds(baselineCase.duration_ms)} · `,
        element('span', {
          class: `change-${delta.direction}`,
          text: `${formatRatioPercent(delta.ratio)} · ${delta.label}`,
        }),
      ),
    );
  }
  facts.appendChild(factBody);
  card.appendChild(element('div', { class: 'table-scroll' }, facts));

  if (timings && timings.units.length > 0) {
    card.appendChild(renderCargoUnits(coldCase));
  }
  return card;
}

/** Lists the slowest compilation units from Cargo's own timings report. */
function renderCargoUnits(coldCase: ColdStartCase): HTMLElement {
  const timings = coldCase.cargo_timings!;
  const table = element('table', { class: 'data-table' });
  table.appendChild(
    tableHead([
      { text: '#', class: 'rank' },
      'Unit',
      'Mode',
      { text: 'Compile time', class: 'numeric' },
      { text: 'Started at', class: 'numeric' },
    ]),
  );
  const body = element('tbody');
  timings.units.slice(0, 25).forEach((unit, index) => {
    body.appendChild(
      tableRow([
        { text: String(index + 1), class: 'rank' },
        element(
          'span',
          {},
          unit.name,
          unit.version ? element('span', { class: 'param-badge', text: unit.version }) : null,
          unit.target ? element('span', { class: 'param-badge', text: unit.target }) : null,
        ),
        unit.mode,
        { text: formatSeconds(unit.duration_seconds), class: 'numeric' },
        { text: formatSeconds(unit.start_seconds), class: 'numeric' },
      ]),
    );
  });
  table.appendChild(body);

  const hint =
    `${timings.unit_count} units · ${formatSeconds(timings.total_seconds)} wall` +
    (timings.units_truncated ? ` · ${timings.units_truncated} slower-tail units omitted` : '');
  return collapsiblePanel(
    'Cargo --timings breakdown (slowest units)',
    hint,
    element('div', { class: 'table-scroll' }, table),
  );
}

// =============================================================================
// Sidebar
// =============================================================================

function renderSidebar(
  buildCases: ColdStartCase[],
  startupCases: ColdStartCase[],
): HTMLElement {
  const container = element('div');
  for (const [label, cases] of [
    ['Compilation', buildCases],
    ['Startup', startupCases],
  ] as [string, ColdStartCase[]][]) {
    if (cases.length === 0) continue;
    const group = element('div', { class: 'toc-group' });
    group.appendChild(element('div', { class: 'toc-group-header', text: label }));
    for (const coldCase of cases) {
      const item = element(
        'a',
        { class: 'toc-item' },
        element('span', { class: 'direction-dot unchanged' }),
        element('span', { text: humanizeIdentifier(coldCase.name) }),
      );
      scrollToOnClick(item, `case-${coldCase.name}`);
      group.appendChild(item);
    }
    container.appendChild(group);
  }
  return container;
}

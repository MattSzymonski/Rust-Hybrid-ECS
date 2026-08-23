// DESCRIPTION: Chart construction for the Pill Lab reports, built on Chart.js.
//
//   The scatter and histogram charts are ports of the ones the previous
//   `gen_bench_report.py` emitted, including its two custom plugins: a shaded
//   confidence-interval band behind the samples and a dashed mean reference
//   line. Chart.js is a bundled npm dependency rather than a CDN script, so the
//   dashboard works offline.
//
// --- SCRIPT ---

import {
  BarController,
  BarElement,
  CategoryScale,
  Chart,
  Legend,
  LinearScale,
  PointElement,
  ScatterController,
  Tooltip,
  type ChartConfiguration,
  type Plugin,
} from 'chart.js';

// Only the controllers actually used are registered, keeping the bundle small.
Chart.register(
  ScatterController,
  BarController,
  PointElement,
  BarElement,
  LinearScale,
  CategoryScale,
  Tooltip,
  Legend,
);

Chart.defaults.font.family = "'Inter', system-ui, -apple-system, sans-serif";
Chart.defaults.font.size = 10;
Chart.defaults.color = 'rgba(255, 255, 255, 0.45)';
Chart.defaults.animation = false;

const GRID_COLOR = 'rgba(128, 128, 128, 0.1)';
const SAMPLE_COLOR = 'rgba(31, 120, 180, 0.35)';
const SAMPLE_BORDER = 'rgba(31, 120, 180, 0.8)';
const OUTLIER_COLOR = 'rgba(214, 39, 40, 0.6)';
const BASELINE_COLOR = 'rgba(128, 128, 128, 0.28)';

/** Extra fields the custom plugins read off a chart configuration. */
interface AnnotatedConfiguration {
  _ciLower?: number;
  _ciUpper?: number;
  _meanValue?: number;
}

/** Shades the 95% confidence interval band behind the sample cloud. */
const confidenceBandPlugin: Plugin = {
  id: 'confidenceBand',
  beforeDatasetsDraw(chart) {
    const config = chart.config as unknown as AnnotatedConfiguration;
    const { _ciLower: lower, _ciUpper: upper } = config;
    if (lower === undefined || upper === undefined || lower === upper) return;

    const yAxis = chart.scales.y;
    const xAxis = chart.scales.x;
    if (!yAxis || !xAxis) return;
    const lowerPixel = yAxis.getPixelForValue(lower);
    const upperPixel = yAxis.getPixelForValue(upper);
    if (Number.isNaN(lowerPixel) || Number.isNaN(upperPixel)) return;

    const context = chart.ctx;
    context.save();
    context.fillStyle = 'rgba(31, 120, 180, 0.10)';
    context.fillRect(
      xAxis.left,
      Math.min(lowerPixel, upperPixel),
      xAxis.right - xAxis.left,
      Math.abs(upperPixel - lowerPixel),
    );
    context.restore();
  },
};

/** Draws the dashed mean reference line with its inline label. */
const meanLinePlugin: Plugin = {
  id: 'meanLine',
  afterDatasetsDraw(chart) {
    const config = chart.config as unknown as AnnotatedConfiguration;
    const meanValue = config._meanValue;
    if (meanValue === undefined) return;

    const yAxis = chart.scales.y;
    const xAxis = chart.scales.x;
    if (!yAxis || !xAxis) return;
    const pixel = yAxis.getPixelForValue(meanValue);
    if (Number.isNaN(pixel) || pixel < yAxis.top || pixel > yAxis.bottom) return;

    const context = chart.ctx;
    context.save();
    context.setLineDash([6, 3]);
    context.strokeStyle = 'rgba(99, 179, 237, 0.65)';
    context.lineWidth = 1;
    context.beginPath();
    context.moveTo(xAxis.left, pixel);
    context.lineTo(xAxis.right, pixel);
    context.stroke();
    context.setLineDash([]);
    context.fillStyle = 'rgba(99, 179, 237, 0.9)';
    context.fillText(`mean ${meanValue.toFixed(2)} µs`, xAxis.right - 100, pixel - 4);
    context.restore();
  },
};

export interface ScatterChartInput {
  label: string;
  samplesMicros: number[];
  outlierFlags: boolean[];
  baselineSamplesMicros: number[];
  baselineLabel: string;
  confidenceLower: number;
  confidenceUpper: number;
  meanMicros: number;
}

/**
 * Builds the per-sample scatter chart: regular samples, IQR outliers drawn as
 * red crosses, and an optional overlay of a comparison run's samples.
 */
export function createScatterChart(
  canvas: HTMLCanvasElement,
  input: ScatterChartInput,
): Chart {
  const regularPoints: { x: number; y: number }[] = [];
  const outlierPoints: { x: number; y: number }[] = [];
  input.samplesMicros.forEach((value, index) => {
    const point = { x: index + 1, y: value };
    if (input.outlierFlags[index]) outlierPoints.push(point);
    else regularPoints.push(point);
  });

  const datasets: ChartConfiguration<'scatter'>['data']['datasets'] = [
    {
      label: input.label,
      data: regularPoints,
      backgroundColor: SAMPLE_COLOR,
      borderColor: SAMPLE_BORDER,
      pointRadius: 2.5,
      pointHoverRadius: 5,
      order: 2,
    },
  ];
  if (outlierPoints.length > 0) {
    datasets.push({
      label: 'Outliers',
      data: outlierPoints,
      backgroundColor: OUTLIER_COLOR,
      borderColor: 'rgba(214, 39, 40, 1)',
      pointRadius: 3,
      pointHoverRadius: 6,
      pointStyle: 'crossRot',
      order: 1,
    });
  }
  const hasBaseline = input.baselineSamplesMicros.length > 0;
  if (hasBaseline) {
    datasets.push({
      label: input.baselineLabel,
      data: input.baselineSamplesMicros.map((value, index) => ({ x: index + 1, y: value })),
      backgroundColor: BASELINE_COLOR,
      borderColor: 'rgba(128, 128, 128, 0.5)',
      pointRadius: 2,
      pointHoverRadius: 4,
      order: 3,
    });
  }

  const configuration: ChartConfiguration<'scatter'> & AnnotatedConfiguration = {
    type: 'scatter',
    data: { datasets },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: {
        x: {
          title: { display: true, text: 'Sample' },
          grid: { color: GRID_COLOR },
        },
        y: {
          title: { display: true, text: 'Time (µs)' },
          beginAtZero: true,
          grid: { color: GRID_COLOR },
        },
      },
      plugins: {
        legend: {
          display: hasBaseline || outlierPoints.length > 0,
          position: 'top',
          labels: { boxWidth: 10, font: { size: 10 } },
        },
        tooltip: {
          callbacks: {
            label: (item) => `${(item.raw as { y: number }).y.toFixed(2)} µs`,
          },
        },
      },
    },
    plugins: [confidenceBandPlugin, meanLinePlugin],
    _ciLower: input.confidenceLower,
    _ciUpper: input.confidenceUpper,
    _meanValue: input.meanMicros,
  };

  return new Chart(canvas, configuration);
}

/**
 * Builds the sample-distribution histogram.
 *
 * The bin count follows Sturges' rule (log2(n) + 1), which is what the
 * original report used and keeps the shape readable for Criterion's default
 * sample size.
 */
export function createHistogramChart(
  canvas: HTMLCanvasElement,
  samplesMicros: number[],
  meanMicros: number,
): Chart | null {
  if (samplesMicros.length === 0) return null;

  const binCount = Math.max(5, Math.ceil(Math.log2(samplesMicros.length) + 1));
  const minimum = Math.min(...samplesMicros);
  const maximum = Math.max(...samplesMicros);
  const span = maximum - minimum || 1;
  const binWidth = span / binCount;

  const bins = new Array<number>(binCount).fill(0);
  const binLabels: string[] = [];
  for (let index = 0; index < binCount; index += 1) {
    binLabels.push((minimum + index * binWidth).toFixed(1));
  }
  for (const value of samplesMicros) {
    const index = Math.min(Math.floor((value - minimum) / binWidth), binCount - 1);
    bins[index] += 1;
  }

  const configuration: ChartConfiguration<'bar'> & AnnotatedConfiguration = {
    type: 'bar',
    data: {
      labels: binLabels,
      datasets: [
        {
          label: 'Frequency',
          data: bins,
          backgroundColor: 'rgba(31, 120, 180, 0.5)',
          borderColor: 'rgba(31, 120, 180, 0.9)',
          borderWidth: 1,
          barPercentage: 1,
          categoryPercentage: 1,
        },
      ],
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      scales: {
        x: {
          title: { display: true, text: 'Time (µs)' },
          grid: { color: GRID_COLOR },
          ticks: { maxTicksLimit: 12 },
        },
        y: {
          title: { display: true, text: 'Count' },
          beginAtZero: true,
          grid: { color: GRID_COLOR },
        },
      },
      plugins: {
        legend: { display: false },
        tooltip: {
          callbacks: { label: (item) => `${item.parsed.y} samples` },
        },
      },
    },
    plugins: [],
    _meanValue: meanMicros,
  };

  return new Chart(canvas, configuration);
}

export interface PhaseBarInput {
  categories: string[];
  series: { label: string; values: number[]; color: string }[];
  axisLabel: string;
  stacked: boolean;
}

/**
 * Builds a horizontal bar chart used by the hot-reload and cold-start reports.
 *
 * Stacked mode renders a reload's build/stage/load/init/migrate phases as one
 * bar per case; unstacked mode compares a current run against its baseline.
 */
export function createBarChart(canvas: HTMLCanvasElement, input: PhaseBarInput): Chart {
  const configuration: ChartConfiguration<'bar'> = {
    type: 'bar',
    data: {
      labels: input.categories,
      datasets: input.series.map((series) => ({
        label: series.label,
        data: series.values,
        backgroundColor: series.color,
        borderWidth: 0,
        barPercentage: 0.75,
        categoryPercentage: 0.8,
      })),
    },
    options: {
      indexAxis: 'y',
      responsive: true,
      maintainAspectRatio: false,
      scales: {
        x: {
          stacked: input.stacked,
          beginAtZero: true,
          title: { display: true, text: input.axisLabel },
          grid: { color: GRID_COLOR },
        },
        y: {
          stacked: input.stacked,
          grid: { display: false },
        },
      },
      plugins: {
        legend: {
          display: input.series.length > 1,
          position: 'top',
          labels: { boxWidth: 10, font: { size: 10 } },
        },
      },
    },
  };
  return new Chart(canvas, configuration);
}

/** Renders a tiny inline SVG sparkline for the sidebar entries. */
export function sparklineSvg(values: number[], width = 80, height = 20): SVGSVGElement | null {
  if (values.length < 2) return null;
  const minimum = Math.min(...values);
  const maximum = Math.max(...values);
  const span = maximum - minimum || 1;
  // Thin long sample arrays so the polyline stays cheap to draw.
  const step = values.length > 200 ? Math.floor(values.length / 200) : 1;
  const thinned = values.filter((_, index) => index % step === 0);

  const xScale = (width - 2) / (thinned.length - 1);
  const yScale = (height - 2) / span;
  const points = thinned
    .map((value, index) => {
      const x = 1 + index * xScale;
      const y = height - 1 - (value - minimum) * yScale;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');

  const svgNamespace = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(svgNamespace, 'svg');
  svg.setAttribute('width', String(width));
  svg.setAttribute('height', String(height));
  svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
  svg.setAttribute('class', 'sparkline-svg');
  svg.setAttribute('aria-hidden', 'true');
  const polyline = document.createElementNS(svgNamespace, 'polyline');
  polyline.setAttribute('fill', 'none');
  polyline.setAttribute('stroke', 'currentColor');
  polyline.setAttribute('stroke-width', '1.2');
  polyline.setAttribute('stroke-linecap', 'round');
  polyline.setAttribute('stroke-linejoin', 'round');
  polyline.setAttribute('opacity', '0.4');
  polyline.setAttribute('points', points);
  svg.appendChild(polyline);
  return svg;
}

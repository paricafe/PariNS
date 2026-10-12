import { useEffect, useRef, useState } from "react";
import { formatDate, formatNumber, translate, type Language } from "../../i18n";
import { isolatedPoints, trendSeries, type HistorySample, type TrendKey, type TrendPoint } from "./metrics";

interface Trace { key: TrendKey; dash?: string; color: string }
// Dash patterns keep the series distinguishable without color.
const traces: readonly Trace[] = [
  { key: "requests", color: "var(--primary)" },
  { key: "cache_hits", dash: "8 5", color: "var(--primary-2)" },
  { key: "blocked", dash: "2 5", color: "var(--danger)" },
];

/** Closed areas under each continuous run of points. */
function areaFor(points: readonly TrendPoint[], x: (time: number) => number, y: (value: number) => number, base: number): string {
  let path = "";
  let run: { time: number; value: number }[] = [];
  const flush = () => {
    if (run.length > 1) path += `M${x(run[0].time).toFixed(2)},${base} ${run.map((point) => `L${x(point.time).toFixed(2)},${y(point.value).toFixed(2)}`).join(" ")} L${x(run[run.length - 1].time).toFixed(2)},${base} Z `;
    run = [];
  };
  for (const point of points) {
    if (point.value === null || point.breakBefore) flush();
    if (point.value !== null) run.push({ time: point.time, value: point.value });
  }
  flush();
  return path;
}

/** Time spans between continuous runs: the service was down, restarted or the clock jumped. */
function gapsFor(points: readonly TrendPoint[]): [number, number][] {
  const gaps: [number, number][] = [];
  let last: number | null = null;
  let open = false;
  for (const point of points) {
    if (point.value === null) { if (last !== null) open = true; continue; }
    if (last !== null && (open || point.breakBefore)) gaps.push([last, point.time]);
    last = point.time; open = false;
  }
  return gaps;
}

function pathFor(points: readonly TrendPoint[], x: (time: number) => number, y: (value: number) => number): string {
  let pen = false;
  let path = "";
  for (const point of points) {
    if (point.value === null) { pen = false; continue; }
    path += `${pen && !point.breakBefore ? "L" : "M"}${x(point.time).toFixed(2)},${y(point.value).toFixed(2)} `;
    pen = true;
  }
  return path;
}

export function TrendChart({ samples, hours, language, end }: { samples: readonly HistorySample[]; hours: number; language: Language; end?: number }) {
  const container = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(770);
  useEffect(() => {
    const element = container.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setWidth(Math.max(220, element.clientWidth)));
    observer.observe(element);
    setWidth(Math.max(220, element.clientWidth));
    return () => observer.disconnect();
  }, [samples.length]);

  const now = end ?? Date.now();
  const series = traces.map((trace) => ({ ...trace, points: trendSeries(samples, trace.key, hours, now) }));
  const requestPoints = series[0].points.filter((point): point is TrendPoint & { value: number } => point.value !== null);
  if (!requestPoints.length) return <p className="chart-empty">{translate("views.chartEmpty", language)}</p>;

  const maximum = Math.max(1, ...series.flatMap((trace) => trace.points.map((point) => point.value ?? 0)));
  const left = width < 400 ? 44 : 58;
  const right = width - 16;
  const start = now - hours * 3_600_000;
  const x = (time: number) => left + (time - start) / (now - start) * (right - left);
  const y = (value: number) => 200 - value / maximum * 170;
  const divisions = width < 400 ? 2 : 4;
  const peak = Math.max(...requestPoints.map((point) => point.value));

  return <div ref={container}>
    <svg role="img" aria-label={translate("views.trendLabel", language, { hours })} viewBox={`0 0 ${width} 244`}
      className="trend-svg">
      <defs><linearGradient id="parins-trend-fill" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stopColor="var(--primary)" stopOpacity="0.28" /><stop offset="1" stopColor="var(--primary)" stopOpacity="0" /></linearGradient></defs>
      {Array.from({ length: divisions + 1 }, (_, index) => {
        const value = maximum * index / divisions;
        return <g key={index}>
          <line x1={left} x2={right} y1={y(value)} y2={y(value)} stroke="currentColor" opacity="0.14" />
          <text x={left - 8} y={y(value) + 4} textAnchor="end" fill="currentColor" opacity="0.65" fontSize="11">
            {formatNumber(value, language, value >= 1000
              ? { notation: "compact", maximumFractionDigits: 1 }
              : { minimumFractionDigits: maximum < 4 ? 2 : 0, maximumFractionDigits: maximum < 4 ? 2 : 0 })}
          </text>
        </g>;
      })}
      {gapsFor(series[0].points).map(([from, to]) => <rect key={from} className="trend-gap" x={x(from)} y={30} width={Math.max(2, x(to) - x(from))} height={170}><title>{translate('shell.gapTip', language)}</title></rect>)}
      <path className="trend-area" d={areaFor(series[0].points, x, y, y(0))} fill="url(#parins-trend-fill)" stroke="none" />
      {series.map((trace) => <g key={trace.key} stroke={trace.color} className={trace.dash ? "trend-area" : undefined}>
        {trace.dash
          ? <path d={pathFor(trace.points, x, y)} fill="none" strokeWidth="2" strokeDasharray={trace.dash} strokeLinecap="round" />
          : <path className="trend-line" d={pathFor(trace.points, x, y)} fill="none" strokeWidth="2.4" strokeLinecap="round" strokeLinejoin="round" pathLength={1} strokeDasharray="1" />}
        {isolatedPoints(trace.points).map((point) => <circle key={point.time} cx={x(point.time)} cy={y(point.value!)} r="3" fill={trace.color} />)}
      </g>)}
      <text x={x(start)} y="227" textAnchor="start" fill="currentColor" opacity="0.65" fontSize="11">{formatDate(start, language, { hour: "2-digit", minute: "2-digit" })}</text>
      <text x={x(now)} y="227" textAnchor="end" fill="currentColor" opacity="0.65" fontSize="11">{formatDate(now, language, { hour: "2-digit", minute: "2-digit" })}</text>
    </svg>
    <p className="muted small">{translate("views.trendSummary", language, {
      count: formatNumber(requestPoints.length, language), peak: formatNumber(peak, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 }),
    })}</p>
  </div>;
}

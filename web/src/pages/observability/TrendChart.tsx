import { useEffect, useRef, useState } from "react";
import { formatDate, formatNumber, translate, type Language } from "../../i18n";
import { isolatedPoints, trendSeries, type HistorySample, type TrendKey, type TrendPoint } from "./metrics";

interface Trace { key: TrendKey; dash?: string; opacity: number }
const traces: readonly Trace[] = [
  { key: "requests", opacity: 1 },
  { key: "cache_hits", dash: "8 5", opacity: 0.7 },
  { key: "blocked", dash: "2 5", opacity: 0.5 },
];

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
  if (!requestPoints.length) return <p className="py-12 text-center text-sm text-zinc-500 dark:text-zinc-400">{translate("views.chartEmpty", language)}</p>;

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
      className="block h-60 w-full text-zinc-900 dark:text-zinc-100">
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
      {series.map((trace) => <g key={trace.key} stroke="currentColor" opacity={trace.opacity}>
        <path d={pathFor(trace.points, x, y)} fill="none" strokeWidth="2" strokeDasharray={trace.dash} />
        {isolatedPoints(trace.points).map((point) => <circle key={point.time} cx={x(point.time)} cy={y(point.value!)} r="3" fill="currentColor" />)}
      </g>)}
      <text x={x(start)} y="227" textAnchor="start" fill="currentColor" opacity="0.65" fontSize="11">{formatDate(start, language, { hour: "2-digit", minute: "2-digit" })}</text>
      <text x={x(now)} y="227" textAnchor="end" fill="currentColor" opacity="0.65" fontSize="11">{formatDate(now, language, { hour: "2-digit", minute: "2-digit" })}</text>
    </svg>
    <p className="mt-2 text-xs text-zinc-500 dark:text-zinc-400">{translate("views.trendSummary", language, {
      count: formatNumber(requestPoints.length, language), peak: formatNumber(peak, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 }),
    })}</p>
  </div>;
}

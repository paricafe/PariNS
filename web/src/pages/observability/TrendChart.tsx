import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { formatDate, formatNumber, translate, type Language } from "../../i18n";
import "./observability.css";
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

/** A hover/keyboard stop: a sample with values, or a span where the service was not running. */
type Stop = { kind: "point"; index: number; time: number } | { kind: "gap"; from: number; to: number; time: number };

function stopsFor(points: readonly TrendPoint[]): Stop[] {
  const stops: Stop[] = [];
  points.forEach((point, index) => { if (point.value !== null) stops.push({ kind: "point", index, time: point.time }); });
  for (const [from, to] of gapsFor(points)) stops.push({ kind: "gap", from, to, time: (from + to) / 2 });
  return stops.sort((a, b) => a.time - b.time);
}

/** Inside a gap shows the gap; anywhere else snaps to the nearest sample. */
function stopAt(stops: readonly Stop[], time: number): Stop | null {
  const gap = stops.find((stop) => stop.kind === "gap" && stop.from < time && time < stop.to);
  if (gap) return gap;
  let best: Stop | null = null;
  for (const stop of stops) if (stop.kind === "point" && (!best || Math.abs(stop.time - time) < Math.abs(best.time - time))) best = stop;
  return best;
}

const HEIGHT = 244;
const rate = (value: number | null, language: Language) => value === null ? "—" : formatNumber(value, language, { maximumFractionDigits: 2 });

export function TrendChart({ samples, hours, language, end }: { samples: readonly HistorySample[]; hours: number; language: Language; end?: number }) {
  const container = useRef<HTMLDivElement>(null);
  const svg = useRef<SVGSVGElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const tipId = useId();
  const hintId = useId();
  const [width, setWidth] = useState(770);
  const [activeTime, setActiveTime] = useState<number | null>(null);
  const tipX = useRef<number | null>(null);
  /** The SVG keeps its aspect ratio, so pointer and tooltip positions map through the rendered scale and centring offset. */
  const geometry = () => {
    const rect = svg.current?.getBoundingClientRect();
    if (!rect?.width || !rect.height) return null;
    const scale = Math.min(rect.width / width, rect.height / HEIGHT);
    return { rect, scale, offset: (rect.width - width * scale) / 2 };
  };
  useEffect(() => {
    const element = container.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setWidth(Math.max(220, element.clientWidth)));
    observer.observe(element);
    setWidth(Math.max(220, element.clientWidth));
    return () => observer.disconnect();
  }, [samples.length]);

  // Keep the tooltip centred on the stop but inside the chart box.
  useLayoutEffect(() => {
    const element = tip.current;
    const box = container.current;
    if (!element || !box || tipX.current === null) return;
    const view = geometry();
    const boxLeft = box.getBoundingClientRect().left;
    const center = view ? view.rect.left - boxLeft + view.offset + tipX.current * view.scale : tipX.current;
    const room = box.clientWidth - element.offsetWidth;
    element.style.left = `${Math.max(0, Math.min(room, center - element.offsetWidth / 2))}px`;
  });

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
  const stops = stopsFor(series[0].points);
  const active = activeTime === null ? null : stops.find((stop) => stop.time === activeTime) ?? null;

  tipX.current = active ? x(active.time) : null;
  const locate = (clientX: number) => {
    const view = geometry();
    if (!view) return;
    const viewX = (clientX - view.rect.left - view.offset) / view.scale;
    const stop = stopAt(stops, start + (viewX - left) / (right - left) * (now - start));
    setActiveTime(stop?.time ?? null);
  };
  const onPointer = (event: PointerEvent<SVGSVGElement>) => {
    // Touch shows on tap only; a mouse or pen follows the pointer.
    if (event.type === "pointermove" && event.pointerType === "touch") return;
    if (event.type === "pointerdown" && event.pointerType !== "touch") return;
    locate(event.clientX);
  };
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "Escape") { if (active) { event.preventDefault(); setActiveTime(null); } return; }
    const current = active ? stops.indexOf(active) : -1;
    const next = event.key === "ArrowLeft" ? (current < 0 ? stops.length - 1 : Math.max(0, current - 1))
      : event.key === "ArrowRight" ? (current < 0 ? 0 : Math.min(stops.length - 1, current + 1))
        : event.key === "Home" ? 0 : event.key === "End" ? stops.length - 1 : null;
    if (next === null) return;
    event.preventDefault();
    setActiveTime(stops[next].time);
  };
  const time = (value: number) => formatDate(value, language, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });

  return <div ref={container} className="trend-chart" tabIndex={0} role="group" aria-label={translate("views.trendLabel", language, { hours })}
    aria-describedby={active ? `${hintId} ${tipId}` : hintId} onKeyDown={onKeyDown} onBlur={() => setActiveTime(null)}>
    <p id={hintId} className="sr-only">{translate("views.chartKeys", language)}</p>
    <svg ref={svg} role="img" aria-label={translate("views.trendLabel", language, { hours })} viewBox={`0 0 ${width} ${HEIGHT}`}
      className="trend-svg" onPointerMove={onPointer} onPointerDown={onPointer}
      onPointerLeave={(event) => { if (event.pointerType !== "touch") setActiveTime(null); }}>
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
      {gapsFor(series[0].points).map(([from, to]) => <rect key={from} className="trend-gap" data-active={active?.kind === "gap" && active.from === from || undefined} x={x(from)} y={30} width={Math.max(2, x(to) - x(from))} height={170}><title>{translate('shell.gapTip', language)}</title></rect>)}
      <path className="trend-area" d={areaFor(series[0].points, x, y, y(0))} fill="url(#parins-trend-fill)" stroke="none" />
      {series.map((trace) => <g key={trace.key} stroke={trace.color} className={trace.dash ? "trend-area" : undefined}>
        {trace.dash
          ? <path d={pathFor(trace.points, x, y)} fill="none" strokeWidth="2" strokeDasharray={trace.dash} strokeLinecap="round" />
          : <path className="trend-line" d={pathFor(trace.points, x, y)} fill="none" strokeWidth="2.4" strokeLinecap="round" strokeLinejoin="round" pathLength={1} strokeDasharray="1" />}
        {isolatedPoints(trace.points).map((point) => <circle key={point.time} cx={x(point.time)} cy={y(point.value!)} r="3" fill={trace.color} />)}
      </g>)}
      {active?.kind === "point" && <g className="trend-cursor" aria-hidden="true">
        <line x1={x(active.time)} x2={x(active.time)} y1={30} y2={200} />
        {series.map((trace) => {
          const value = trace.points[active.index].value;
          return value === null ? null : <circle key={trace.key} cx={x(active.time)} cy={y(value)} r="4.5" fill={trace.color} />;
        })}
      </g>}
      <text x={x(start)} y="227" textAnchor="start" fill="currentColor" opacity="0.65" fontSize="11">{formatDate(start, language, { hour: "2-digit", minute: "2-digit" })}</text>
      <text x={x(now)} y="227" textAnchor="end" fill="currentColor" opacity="0.65" fontSize="11">{formatDate(now, language, { hour: "2-digit", minute: "2-digit" })}</text>
    </svg>
    {active && <div ref={tip} id={tipId} role="tooltip" className="glass trend-tip">
      {active.kind === "gap"
        ? <><p className="trend-tip-time">{time(active.from)} – {time(active.to)}</p><p>{translate("views.chartGap", language)}</p></>
        : <><p className="trend-tip-time">{time(active.time)}</p>
          <dl>{series.map((trace, index) => <div key={trace.key} data-series={index}>
            <dt>{translate(["views.tipRequests", "views.tipCacheHits", "views.tipBlocked"][index], language)}</dt>
            <dd>{rate(trace.points[active.index].value, language)}</dd>
          </div>)}</dl></>}
    </div>}
    <p className="muted small">{translate("views.trendSummary", language, {
      count: formatNumber(requestPoints.length, language), peak: formatNumber(peak, language, { minimumFractionDigits: 2, maximumFractionDigits: 2 }),
    })}</p>
  </div>;
}

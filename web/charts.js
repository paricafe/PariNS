"use strict";

globalThis.PariCharts = (() => {
  const format = (value) => Number.isFinite(value) ? new Intl.NumberFormat("zh-CN").format(value) : "—";
  function series(samples, key, hours, now = Date.now()) {
    return samples.filter((sample) => sample.timestamp_ms >= now - hours * 3600000 && sample.timestamp_ms <= now).map((sample, index, all) => ({
      time: sample.timestamp_ms,
      value: sample.running && sample.elapsed_seconds > 0 ? sample[key] / sample.elapsed_seconds : null,
      breakBefore: index === 0 || !all[index - 1].running || sample.generation !== all[index - 1].generation || sample.timestamp_ms - all[index - 1].timestamp_ms > 90000
    }));
  }
  function histogram(buckets) {
    let previous = 0, lower = 0;
    return buckets.map((bucket) => {
      const count = Math.max(0, bucket.count - previous); previous = bucket.count;
      const label = bucket.upper_bound_micros === null ? `> ${lower} ms` : lower === 0 ? `≤ ${bucket.upper_bound_micros / 1000} ms` : `> ${lower}–${bucket.upper_bound_micros / 1000} ms`;
      lower = bucket.upper_bound_micros / 1000;
      return { label, count };
    });
  }
  const svgNode = (tag, attributes = {}) => {
    const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, String(value));
    return node;
  };
  function trend(container, samples, hours) {
    container.replaceChildren();
    const now = Date.now();
    const traces = [["requests", "请求", "request-line"], ["cache_hits", "缓存命中", "cache-line"], ["blocked", "拦截", "blocked-line"]].map(([key, title, className]) => ({ title, className, points: series(samples, key, hours, now) }));
    const points = traces[0].points.filter((point) => point.value !== null);
    if (!points.length) {
      const empty = document.createElement("p"); empty.className = "chart-empty"; empty.textContent = "暂无可用采样。DNS 运行后每分钟记录一次；这里不生成演示数据。"; container.append(empty); return;
    }
    const maximum = Math.max(1, ...traces.flatMap((trace) => trace.points.map((point) => point.value ?? 0)));
    const start = now - hours * 3600000;
    const width = Math.max(220, container.clientWidth || 770);
    const left = width < 400 ? 42 : 54, right = width - 16;
    const x = (time) => left + ((time - start) / (now - start)) * (right - left);
    const y = (value) => 200 - (value / maximum) * 170;
    const svg = svgNode("svg", { viewBox: `0 0 ${width} 244`, role: "img", "aria-label": `最近 ${hours} 小时每秒请求、缓存命中与拦截趋势；断线表示服务中断或实例重启。` });
    const divisions = width < 400 ? 2 : 4;
    for (let index = 0; index <= divisions; index += 1) {
      const value = (maximum * index) / divisions;
      svg.append(svgNode("line", { x1: left, x2: right, y1: y(value), y2: y(value), class: "chart-grid" }));
      const label = svgNode("text", { x: left - 8, y: y(value) + 4, "text-anchor": "end", class: "chart-label" }); label.textContent = value >= 1000 ? new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 1 }).format(value) : value.toFixed(maximum < 4 ? 2 : 0); svg.append(label);
    }
    for (const trace of traces) {
      let d = "", pen = false;
      for (const point of trace.points) {
        if (point.value === null) { pen = false; continue; }
        d += `${pen && !point.breakBefore ? "L" : "M"}${x(point.time).toFixed(2)},${y(point.value).toFixed(2)} `; pen = true;
      }
      svg.append(svgNode("path", { d, class: trace.className, fill: "none", "stroke-width": 2 }));
      if (trace.points.length < 3) for (const point of trace.points.filter((point) => point.value !== null)) svg.append(svgNode("circle", { cx: x(point.time), cy: y(point.value), r: 3, class: trace.className }));
    }
    for (const [time, anchor] of [[start, "start"], [now, "end"]]) { const label = svgNode("text", { x: x(time), y: 227, "text-anchor": anchor, class: "chart-label" }); label.textContent = new Date(time).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" }); svg.append(label); }
    container.append(svg);
    const summary = document.createElement("p"); summary.className = "muted small";
    summary.textContent = `${points.length} 个有效采样 · 请求峰值 ${Math.max(...points.map((point) => point.value)).toFixed(2)} 次/秒。断线表示不可用区间或 DNS 实例切换。`;
    container.append(summary);
  }
  function table(container, rows, unavailable = false) {
    container.replaceChildren();
    if (unavailable) { const note = document.createElement("p"); note.className = "muted"; note.textContent = "当前实例统计不可用。"; container.append(note); return; }
    const table = document.createElement("table"); table.className = "distribution";
    const thead = document.createElement("thead"); const headers = document.createElement("tr");
    for (const label of ["类别", "次数", "分布"]) { const th = document.createElement("th"); th.scope = "col"; th.textContent = label; headers.append(th); }
    thead.append(headers); table.append(thead);
    const body = document.createElement("tbody"); const total = rows.reduce((sum, row) => sum + row.count, 0);
    for (const row of rows) {
      const tr = document.createElement("tr"); const label = document.createElement("th"); label.scope = "row"; label.textContent = row.label;
      const count = document.createElement("td"); count.textContent = format(row.count);
      const bar = document.createElement("td"); const meter = document.createElement("meter"); meter.min = 0; meter.max = total || 1; meter.value = row.count; meter.setAttribute("aria-label", `${row.label}占比 ${total ? (row.count * 100 / total).toFixed(1) : 0}%`); bar.append(meter); tr.append(label, count, bar); body.append(tr);
    }
    table.append(body); container.append(table);
  }
  return { format, series, histogram, trend, table };
})();

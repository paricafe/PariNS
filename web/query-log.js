"use strict";

globalThis.PariQueryLog = (() => {
  const I = PariI18n, t = (key, params) => I.t(`views.${key}`, params);
  const statuses = Object.defineProperties({}, Object.fromEntries(["success", "blocked", "error", "dropped"].map(key => [key, { get: () => t(key), enumerable: true }])));
  const paths = Object.defineProperties({}, Object.fromEntries(Object.entries({ upstream: "pathUpstream", fresh: "pathFresh", stale: "pathStale", blocked: "pathBlocked", error: "pathError", cancelled: "pathCancelled" }).map(([key, value]) => [key, { get: () => t(value), enumerable: true }])));
  function node(tag, text, className) {
    const element = document.createElement(tag);
    if (text !== undefined) element.textContent = String(text);
    if (className) element.className = className;
    return element;
  }
  const label = (tag, key, params, className) => I.bind(node(tag, undefined, className), `views.${key}`, params);
  const value = (tag, getValue) => label(tag, "value", () => ({ value: getValue() }));
  function detail(entry) {
    const details = node("details", undefined, "query-detail");
    details.append(label("summary", "details"));
    const fields = [
      ["recordId", () => entry.id], ["transport", () => entry.transport], ["rcode", () => entry.rcode || t("noResponse")],
      ["path", () => paths[entry.cache] || entry.cache], ["upstream", () => entry.upstream || t("none")],
      ["incomingEcs", () => entry.incoming_ecs || t("none")], ["outgoingEcs", () => entry.outgoing_ecs || t("none")],
      ["flags", () => `EDNS=${entry.edns} · DO=${entry.dnssec_ok} · CD=${entry.checking_disabled} · RD=${entry.recursion_desired}`]
    ];
    const list = node("dl", undefined, "query-detail-list");
    for (const [title, getValue] of fields) list.append(label("dt", title), value("dd", getValue));
    details.append(list, label("h3", "answers"));
    const answers = (entry.answer || []).map(record => `${record.name} ${record.ttl} ${record.record_type} ${record.data}`).join("\n");
    details.append(answers ? node("pre", answers, "query-answers") : label("pre", "noAnswers", {}, "query-answers"));
    if (entry.answer_truncated) details.append(label("p", "answerTruncated", {}, "muted small"));
    return details;
  }
  function render(container, page) {
    container.replaceChildren();
    if (!page.enabled) { container.append(label("p", "logsDisabled", {}, "chart-empty")); return; }
    if (!page.entries.length) { container.append(label("p", "logsEmpty", {}, "chart-empty")); return; }
    const wrap = node("div", undefined, "query-table-wrap");
    wrap.tabIndex = 0; wrap.setAttribute("role", "region"); I.bind(wrap, "views.logsRegion", {}, "aria-label");
    const table = node("table", undefined, "query-table");
    table.append(label("caption", "logsCaption", {}, "sr-only"));
    const head = node("thead"), titles = node("tr");
    for (const title of ["time", "query", "clientTransport", "resultPath", "duration", "details"]) { const th = label("th", title); th.scope = "col"; titles.append(th); }
    head.append(titles); table.append(head);
    const body = node("tbody");
    for (const entry of page.entries) {
      const row = node("tr");
      row.append(value("td", () => I.date(entry.time_ms, { dateStyle: "short", timeStyle: "medium" })), value("td", () => `${entry.name || t("unreadableQuery")}\n${entry.qtype || "—"}`), node("td", `${entry.client}\n${entry.transport}`), value("td", () => `${statuses[entry.status] || entry.status}\n${paths[entry.cache] || entry.cache}`), value("td", () => `${I.number(Number(entry.duration_ms), { minimumFractionDigits: 2, maximumFractionDigits: 2 })} ms`));
      const more = node("td"); more.append(detail(entry)); row.append(more); body.append(row);
    }
    table.append(body); wrap.append(table); container.append(wrap);
  }
  return { render, statuses, paths };
})();

"use strict";

globalThis.PariQueryLog = (() => {
  const statuses = { success: "成功", blocked: "已拦截", error: "错误", dropped: "丢弃" };
  const paths = { upstream: "上游解析", fresh: "缓存命中", stale: "过期缓存兜底", blocked: "过滤规则", error: "请求失败", cancelled: "请求已取消" };
  function node(tag, text, className) {
    const element = document.createElement(tag);
    if (text !== undefined) element.textContent = String(text);
    if (className) element.className = className;
    return element;
  }
  function detail(entry) {
    const details = node("details", undefined, "query-detail");
    details.append(node("summary", "详情"));
    const fields = [
      ["记录编号", entry.id], ["传输", entry.transport], ["响应码", entry.rcode || "无响应"],
      ["处理路径", paths[entry.cache] || entry.cache], ["实际响应上游", entry.upstream || "无（本次未取得上游响应）"],
      ["客户端 ECS", entry.incoming_ecs || "无"], ["出站 ECS", entry.outgoing_ecs || "无"],
      ["请求标志", `EDNS=${entry.edns} · DO=${entry.dnssec_ok} · CD=${entry.checking_disabled} · RD=${entry.recursion_desired}`]
    ];
    const list = node("dl", undefined, "query-detail-list");
    for (const [label, value] of fields) list.append(node("dt", label), node("dd", value));
    details.append(list, node("h3", "答案"));
    const answers = (entry.answer || []).map(record => `${record.name} ${record.ttl} ${record.record_type} ${record.data}`).join("\n");
    details.append(node("pre", answers || "无 Answer 记录", "query-answers"));
    if (entry.answer_truncated) details.append(node("p", "展示长度已截断，不影响实际 DNS 响应。", "muted small"));
    return details;
  }
  function render(container, page) {
    container.replaceChildren();
    if (!page.enabled) { container.append(node("p", "日志尚未启用。点击「日志设置」，开启逐请求日志并保存后开始记录。", "chart-empty")); return; }
    if (!page.entries.length) { container.append(node("p", "没有匹配的记录。可刷新、调整筛选，或发送新的 DNS 请求。", "chart-empty")); return; }
    const wrap = node("div", undefined, "query-table-wrap");
    wrap.tabIndex = 0; wrap.setAttribute("role", "region"); wrap.setAttribute("aria-label", "查询记录，可横向滚动");
    const table = node("table", undefined, "query-table");
    table.append(node("caption", "最新查询优先；展开详情查看答案与 ECS 信息", "sr-only"));
    const head = node("thead"), titles = node("tr");
    for (const title of ["时间", "查询", "客户端 / 传输", "结果 / 路径", "耗时", "详情"]) { const th = node("th", title); th.scope = "col"; titles.append(th); }
    head.append(titles); table.append(head);
    const body = node("tbody");
    for (const entry of page.entries) {
      const row = node("tr");
      const values = [new Date(entry.time_ms).toLocaleString("zh-CN"), `${entry.name || "无法解析"}\n${entry.qtype || "—"}`, `${entry.client}\n${entry.transport}`, `${statuses[entry.status] || entry.status}\n${paths[entry.cache] || entry.cache}`, `${Number(entry.duration_ms).toFixed(2)} ms`];
      for (const value of values) row.append(node("td", value));
      const more = node("td"); more.append(detail(entry)); row.append(more); body.append(row);
    }
    table.append(body); wrap.append(table); container.append(wrap);
  }
  return { render, statuses, paths };
})();

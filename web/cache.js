"use strict";

// Presentation only: matching, validation and cache state remain server-owned.
globalThis.PariCache = (() => {
  const node = (tag, text, className) => {
    const item = document.createElement(tag);
    if (text !== undefined) item.textContent = text;
    if (className) item.className = className;
    return item;
  };
  const defaults = () => ({ name: "", suffix: false, qtype: null, bypass: false, max_ttl_secs: null, negative_ttl_cap_secs: null, prefetch: null, stale: null });
  function decodeRule(values) {
    const result = { name: values.name.trim(), suffix: values.suffix === "true", qtype: values.qtype.trim().toUpperCase() || null, bypass: values.bypass === "true" };
    for (const key of ["max_ttl_secs", "negative_ttl_cap_secs"]) {
      const text = values[key].trim();
      if (text && (!Number.isSafeInteger(Number(text)) || Number(text) < 1 || Number(text) > 86400)) throw new Error("规则 TTL 必须为 1–86400 的整数，留空表示继承。");
      result[key] = text ? Number(text) : null;
    }
    for (const key of ["prefetch", "stale"]) result[key] = values[key] === "" ? null : values[key] === "true";
    if (!result.name) throw new Error("缓存规则必须填写域名。");
    return result;
  }
  function rules(container, entries, onChange) {
    container.replaceChildren();
    const list = node("div");
    const labels = { name: "域名", suffix: "匹配方式", qtype: "记录类型（空为全部）", bypass: "缓存行为", max_ttl_secs: "正向 TTL 上限（空为继承）", negative_ttl_cap_secs: "否定 TTL 上限（空为继承）", prefetch: "预取", stale: "过期兜底" };
    const choices = { suffix: [["false", "精确匹配"], ["true", "本域及子域"]], bypass: [["false", "允许缓存"], ["true", "绕过缓存"]], prefetch: [["", "继承全局"], ["true", "开启"], ["false", "关闭"]], stale: [["", "继承全局"], ["true", "开启"], ["false", "关闭"]] };
    let serial = 0;
    function add(entry) {
      const row = node("fieldset", undefined, "panel cache-rule");
      row.append(node("legend", `规则 ${++serial}`));
      const fields = node("div", undefined, "settings-fields");
      for (const [key, label] of Object.entries(labels)) {
        const wrap = node("div", undefined, "field");
        const input = node(choices[key] ? "select" : "input");
        input.dataset.ruleField = key; input.id = `cache-rule-${serial}-${key}`;
        if (choices[key]) for (const [value, title] of choices[key]) { const option = node("option", title); option.value = value; input.append(option); }
        else { input.type = key.endsWith("_secs") ? "number" : "text"; input.autocomplete = "off"; input.spellcheck = false; }
        if (input.type === "number") { input.min = "1"; input.max = "86400"; input.step = "1"; }
        input.value = entry[key] == null ? "" : String(entry[key]);
        const heading = node("label", label); heading.htmlFor = input.id;
        input.addEventListener("input", onChange); wrap.append(heading, input); fields.append(wrap);
      }
      const remove = node("button", "删除此规则", "button quiet"); remove.type = "button";
      remove.addEventListener("click", () => { row.remove(); onChange(); addButton.focus(); });
      row.append(fields, remove); list.append(row);
    }
    const addButton = node("button", "添加缓存规则", "button secondary"); addButton.type = "button";
    addButton.addEventListener("click", () => { if (list.children.length >= 256) return; add(defaults()); onChange(); list.lastElementChild.querySelector("input").focus(); });
    container.append(list, addButton);
    entries.forEach(add);
    return () => [...list.children].map((row) => decodeRule(Object.fromEntries([...row.querySelectorAll("[data-rule-field]")].map((input) => [input.dataset.ruleField, input.value]))));
  }
  function renderStats(container, cache, refresh) {
    container.replaceChildren();
    if (!cache) { container.append(node("p", "DNS 未运行或缓存统计暂不可用。", "muted")); return; }
    for (const [label, used, max] of [["条目预算使用", cache.entries, cache.max_entries], ["记账字节预算使用", cache.bytes, cache.max_bytes]]) {
      const meter = node("meter"); meter.min = 0; meter.max = max; meter.value = used;
      meter.setAttribute("aria-label", `${label} ${(100 * used / max).toFixed(1)}%`);
      const row = node("div", undefined, "detail-row"); row.append(node("span", label), meter); container.append(row);
    }
    const pairs = [["条目 / 上限", `${cache.entries} / ${cache.max_entries}`], ["记账字节 / 预算", `${cache.bytes} / ${cache.max_bytes}`], ["正向 / 否定条目", `${cache.positive_entries} / ${cache.negative_entries}`], ["新鲜命中 / 过期命中", `${cache.hits} / ${cache.stale_hits}`], ["未命中 / 绕过", `${cache.misses} / ${cache.bypasses}`], ["淘汰 / 拒绝写入", `${cache.evictions} / ${cache.rejections}`], ["分片 / 失效代次", `${cache.shards} / ${cache.epoch}`]];
    if (refresh) for (const [key, label] of [["active", "当前后台刷新"], ["scheduled", "已调度刷新"], ["success", "刷新成功"], ["failure", "刷新失败"], ["rejected", "刷新受限 / 合并"]]) pairs.push([label, refresh[key] ?? 0]);
    for (const [label, value] of pairs) { const row = node("div", undefined, "detail-row"); row.append(node("span", label), node("span", String(value))); container.append(row); }
  }
  const reasons = { fresh: "命中新鲜缓存", stale: "仅有保留的过期结果，正常请求仍先回源", miss: "当前查询条件下未命中", bypass: "此查询绕过缓存" };
  function renderInspection(container, data) {
    container.replaceChildren();
    const result = data.explanation;
    container.append(node("p", reasons[result.state] || result.state));
    container.append(node("p", `判定：${result.reason}；匹配范围：${result.scope || "无"}`, "muted small"));
    if (result.policy) {
      const p = result.policy;
      container.append(node("p", `策略：${p.rule_index == null ? "全局默认" : `第 ${p.rule_index + 1} 条规则`} · TTL 上限 ${p.max_ttl_secs}s · 否定上限 ${p.negative_ttl_cap_secs}s · 预取 ${p.prefetch ? "开" : "关"} · 过期兜底 ${p.stale ? "开" : "关"}`, "small"));
    }
    const variants = data.inspection.variants;
    if (!variants.length) { container.append(node("p", "没有已保留的变体；未命中不会触发网络查询。", "muted")); return; }
    const wrap = node("div", undefined, "cache-table-wrap");
    const table = node("table", undefined, "cache-table");
    const caption = node("caption", "该域名和记录类型的缓存变体（包含其他查询标志）"); table.append(caption);
    const header = node("tr");
    for (const label of ["ECS 范围", "状态", "新鲜 / 保留剩余秒", "类别", "查询标志", "命中", "记账字节"]) { const th = node("th", label); th.scope = "col"; header.append(th); }
    const head = node("thead"); head.append(header); table.append(head);
    const body = node("tbody");
    for (const item of variants) {
      const row = node("tr");
      for (const value of [item.scope, item.state, `${item.fresh_remaining_secs} / ${item.retention_remaining_secs}`, item.negative ? "否定" : "正向", `EDNS=${+item.edns} DO=${+item.dnssec_ok} CD=${+item.checking_disabled} RD=${+item.recursion_desired}`, item.hits, item.bytes]) row.append(node("td", String(value)));
      body.append(row);
    }
    table.append(body); wrap.append(table); container.append(wrap);
    if (data.inspection.truncated) container.append(node("p", "结果已达到展示上限，仅展示部分变体。", "muted"));
  }
  return { defaults, decodeRule, rules, renderStats, renderInspection };
})();

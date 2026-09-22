"use strict";

// Presentation only: matching, validation and cache state remain server-owned.
globalThis.PariCache = (() => {
  const I = PariI18n, t = (key, params) => I.t(`views.${key}`, params);
  const node = (tag, text, className) => {
    const item = document.createElement(tag);
    if (text !== undefined) item.textContent = text;
    if (className) item.className = className;
    return item;
  };
  const label = (tag, key, params, className) => I.bind(node(tag, undefined, className), `views.${key}`, params);
  const value = (tag, getValue, className) => label(tag, "value", () => ({ value: getValue() }), className);
  const scope = (text) => ["no_ecs", "privacy_v4", "privacy_v6"].includes(text) ? t(text) : text || t("none");
  const state = (text) => ["fresh", "expired"].includes(text) ? t(text) : text === "stale" ? t("staleState") : text;
  const defaults = () => ({ name: "", suffix: false, qtype: null, bypass: false, max_ttl_secs: null, negative_ttl_cap_secs: null, prefetch: null, stale: null });
  function decodeRule(values) {
    const result = { name: values.name.trim(), suffix: values.suffix === "true", qtype: values.qtype.trim().toUpperCase() || null, bypass: values.bypass === "true" };
    for (const key of ["max_ttl_secs", "negative_ttl_cap_secs"]) {
      const text = values[key].trim();
      if (text && (!Number.isSafeInteger(Number(text)) || Number(text) < 1 || Number(text) > 86400)) throw new I.MessageError("views.ttlError");
      result[key] = text ? Number(text) : null;
    }
    for (const key of ["prefetch", "stale"]) result[key] = values[key] === "" ? null : values[key] === "true";
    if (!result.name) throw new I.MessageError("views.domainError");
    return result;
  }
  function rules(container, entries, onChange) {
    container.replaceChildren();
    const list = node("div");
    const labels = { name: "name", suffix: "match", qtype: "qtype", bypass: "behavior", max_ttl_secs: "positiveTtl", negative_ttl_cap_secs: "negativeTtl", prefetch: "prefetch", stale: "stale" };
    const choices = { suffix: [["false", "exact"], ["true", "suffix"]], bypass: [["false", "allow"], ["true", "bypass"]], prefetch: [["", "inherit"], ["true", "on"], ["false", "off"]], stale: [["", "inherit"], ["true", "on"], ["false", "off"]] };
    let serial = 0;
    function add(entry) {
      const row = node("fieldset", undefined, "panel cache-rule");
      row.append(label("legend", "rule", { number: ++serial }));
      const fields = node("div", undefined, "settings-fields");
      for (const [key, title] of Object.entries(labels)) {
        const wrap = node("div", undefined, "field");
        const input = node(choices[key] ? "select" : "input");
        input.dataset.ruleField = key; input.id = `cache-rule-${serial}-${key}`;
        if (choices[key]) for (const [value, choice] of choices[key]) { const option = label("option", choice); option.value = value; input.append(option); }
        else { input.type = key.endsWith("_secs") ? "number" : "text"; input.autocomplete = "off"; input.spellcheck = false; }
        if (input.type === "number") { input.min = "1"; input.max = "86400"; input.step = "1"; }
        input.value = entry[key] == null ? "" : String(entry[key]);
        const heading = label("label", title); heading.htmlFor = input.id;
        input.addEventListener("input", onChange); wrap.append(heading, input); fields.append(wrap);
      }
      const remove = label("button", "removeRule", {}, "button quiet"); remove.type = "button";
      remove.addEventListener("click", () => { row.remove(); onChange(); addButton.focus(); });
      row.append(fields, remove); list.append(row);
    }
    const addButton = label("button", "addRule", {}, "button secondary"); addButton.type = "button";
    addButton.addEventListener("click", () => { if (list.children.length >= 256) return; add(defaults()); onChange(); list.lastElementChild.querySelector("input").focus(); });
    container.append(list, addButton);
    entries.forEach(add);
    return () => [...list.children].map((row) => {
      const inputs = [...row.querySelectorAll("[data-rule-field]")];
      for (const input of inputs) {
        // Incomplete native number edits can expose value="" without being empty.
        // Validate before decodeRule interprets an empty value as inheritance.
        if (input.type === "number" && !input.checkValidity()) {
          const error = new I.MessageError("views.ttlError");
          error.fieldId = input.id; error.fieldView = "cache";
          throw error;
        }
      }
      return decodeRule(Object.fromEntries(inputs.map(input => [input.dataset.ruleField, input.value])));
    });
  }
  function renderStats(container, cache, refresh) {
    container.replaceChildren();
    if (!cache) { container.append(label("p", "statsUnavailable", {}, "muted")); return; }
    for (const [title, used, max] of [["entryBudget", cache.entries, cache.max_entries], ["byteBudget", cache.bytes, cache.max_bytes]]) {
      const meter = node("meter"); meter.min = 0; meter.max = max; meter.value = used;
      I.bind(meter, "views.usage", () => ({ label: t(title), percent: I.number(100 * used / max, { maximumFractionDigits: 1 }) }), "aria-label");
      const row = node("div", undefined, "detail-row"); row.append(label("span", title), meter); container.append(row);
    }
    const pairs = [["entriesLimit", [cache.entries, cache.max_entries]], ["bytesLimit", [cache.bytes, cache.max_bytes]], ["positiveNegative", [cache.positive_entries, cache.negative_entries]], ["freshStale", [cache.hits, cache.stale_hits]], ["missBypass", [cache.misses, cache.bypasses]], ["evictionRejection", [cache.evictions, cache.rejections]], ["shardsEpoch", [cache.shards, cache.epoch]]];
    if (refresh) for (const [key, title] of [["active", "activeRefresh"], ["scheduled", "scheduledRefresh"], ["success", "successfulRefresh"], ["failure", "failedRefresh"], ["rejected", "rejectedRefresh"]]) pairs.push([title, [refresh[key] ?? 0]]);
    for (const [title, values] of pairs) { const row = node("div", undefined, "detail-row"); row.append(label("span", title), value("span", () => values.map(item => I.number(item)).join(" / "))); container.append(row); }
  }
  const reasons = { fresh: "explainFresh", stale: "explainStale", miss: "explainMiss", bypass: "explainBypass" };
  function renderInspection(container, data) {
    container.replaceChildren();
    const result = data.explanation;
    container.append(reasons[result.state] ? label("p", reasons[result.state]) : node("p", result.state));
    container.append(label("p", "explanation", () => ({ reason: t(result.reason), scope: scope(result.scope) }), "muted small"));
    if (result.policy) {
      const p = result.policy;
      container.append(label("p", "policy", () => ({ rule: p.rule_index == null ? t("global") : t("rule", { number: p.rule_index + 1 }), ttl: I.number(p.max_ttl_secs), negative: I.number(p.negative_ttl_cap_secs), prefetch: t(p.prefetch ? "on" : "off"), stale: t(p.stale ? "on" : "off") }), "small"));
    }
    const variants = data.inspection.variants;
    if (!variants.length) { container.append(label("p", "noVariants", {}, "muted")); return; }
    const wrap = node("div", undefined, "cache-table-wrap");
    const table = node("table", undefined, "cache-table");
    const caption = label("caption", "variants"); table.append(caption);
    const header = node("tr");
    for (const title of ["scope", "state", "remaining", "category", "flags", "hits", "bytes"]) { const th = label("th", title); th.scope = "col"; header.append(th); }
    const head = node("thead"); head.append(header); table.append(head);
    const body = node("tbody");
    for (const item of variants) {
      const row = node("tr");
      row.append(value("td", () => scope(item.scope)), value("td", () => state(item.state)), value("td", () => `${I.number(item.fresh_remaining_secs)} / ${I.number(item.retention_remaining_secs)}`), label("td", item.negative ? "negative" : "positive"), node("td", `EDNS=${+item.edns} DO=${+item.dnssec_ok} CD=${+item.checking_disabled} RD=${+item.recursion_desired}`), value("td", () => I.number(item.hits)), value("td", () => I.number(item.bytes)));
      body.append(row);
    }
    table.append(body); wrap.append(table); container.append(wrap);
    if (data.inspection.truncated) container.append(label("p", "variantsTruncated", {}, "muted"));
  }
  return { defaults, decodeRule, rules, renderStats, renderInspection };
})();

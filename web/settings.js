"use strict";

// Form metadata is presentation only. Rust remains the parser and validator.
globalThis.PariSettings = (() => {
  const I = globalThis.PariI18n;
  const t = (key, params) => I.t(key, params);
  const field = (path, type = "text", hasHelp = false, min, max, labelPath = path) => ({
    path, type, min, max, labelKey: `settings.${labelPath}.label`, helpKey: hasHelp ? `settings.${path}.help` : null,
    get label() { return t(this.labelKey); },
    get help() { return this.helpKey ? t(this.helpKey) : ""; }
  });
  const number = (path, min, max, help = false) => field(path, "number", help, min, max);
  const select = (path, values) => Object.assign(field(path, "select"), {
    options: values.map(value => [value, `settings.${path}.${value}`])
  });
  const group = (id, fields, optional) => ({
    titleKey: `settings.group.${id}.title`, helpKey: `settings.group.${id}.help`, enableKey: `settings.group.${id}.enable`, fields, optional,
    get title() { return t(this.titleKey); }, get help() { return t(this.helpKey); }
  });
  const page = (id, groups) => ({
    titleKey: `settings.page.${id}.title`, introKey: `settings.page.${id}.intro`, groups,
    get title() { return t(this.titleKey); }, get intro() { return t(this.introKey); }
  });
  const pages = {
    dns: page("dns", [
      group("upstreams", [
        field("upstreams.servers", "lines", true),
        select("upstreams.mode", ["weighted", "parallel"]),
        field("upstreams.prefer_h3", "checkbox", true),
        field("upstreams.bootstrap", "lines", true),
        number("upstreams.max_parallel", 1, 32, true),
        number("upstreams.max_extra_inflight", 1, 65536, true),
        field("upstreams.ca_file", "nullable"),
        field("upstreams.dot_pool.enabled", "checkbox", true),
        number("upstreams.dot_pool.max_connections", 1, 256, true),
        number("upstreams.dot_pool.idle_timeout_ms", 1, 600000)
      ]),
      group("listeners", [
        field("listen", "text", true),
        number("query_timeout_ms", 1, 60000), number("tcp_io_timeout_ms", 1, 60000)
      ])
    ]),
    cache: page("cache", [
      group("cache", [
        field("cache.enabled", "checkbox"), number("cache.max_entries", 1, 262144),
        number("cache.max_bytes", 512, 1073741824, true), number("cache.max_variants", 1, 256),
        number("cache.shards", 1, 64, true), number("cache.negative_percent", 0, 90, true),
        number("cache.max_ttl_secs", 1, 86400), number("cache.negative_ttl_cap_secs", 1, 86400)
      ]),
      group("prefetch", [
        field("cache.prefetch.enabled", "checkbox"), number("cache.prefetch.min_hits", 1, 1000000),
        number("cache.prefetch.remaining_percent", 1, 90), number("cache.prefetch.max_inflight", 1, 256),
        number("cache.prefetch.rate_per_sec", 1, 10000), number("cache.prefetch.backoff_secs", 1, 3600)
      ]),
      group("stale", [
        field("cache.stale.enabled", "checkbox"), number("cache.stale.retention_secs", 1, 604800),
        number("cache.stale.reply_ttl_secs", 1, 300)
      ]),
      group("ecs", [
        field("ecs.enabled", "checkbox"), number("ecs.ipv4_prefix", 0, 32, true), number("ecs.ipv6_prefix", 0, 128)
      ])
    ]),
    filters: page("filters", [
      group("filterSource", [field("filter_file", "nullable", true)]),
      group("filterRules", [
        field("filter.enabled", "checkbox"), field("filter.block_exact", "lines", true),
        field("filter.block_suffix", "lines", true), field("filter.allow_exact", "lines"), field("filter.allow_suffix", "lines")
      ])
    ]),
    security: page("security", [
      ...["dot", "doh", "doq", "doh3"].map(key => group(key,
        ["listen", "cert_file", "key_file"].map(name => field(`${key}.${name}`, "text", false, undefined, undefined, `listener.${name}`)), key))
    ]),
    runtime: page("runtime", [
      group("queryLog", [
        field("query_log.enabled", "checkbox"), number("query_log.max_entries", 1, 10000), number("query_log.retention_secs", 1, 604800)
      ]),
      group("concurrency", [
        number("max_inflight", 1, 65536), number("max_tcp_connections", 1, 65536), number("shutdown_grace_ms", 1, 60000)
      ]),
      group("sourceLimits", [
        field("source_limits.enabled", "checkbox"), number("source_limits.rate_per_sec", 1, 1000000),
        number("source_limits.burst", 1, 1000000), number("source_limits.max_sources", 1, 65536),
        number("source_limits.ipv4_prefix", 0, 32), number("source_limits.ipv6_prefix", 0, 128),
        number("source_limits.max_inflight", 1, 65536), number("source_limits.max_connections", 1, 65536)
      ]),
      group("coalescing", [
        field("coalescing.enabled", "checkbox"), number("coalescing.max_groups", 1, 65536), number("coalescing.max_waiters", 1, 65536)
      ]),
      group("metrics", [
        number("metrics.interval_secs", 0, 3600, true), field("admin_listen", "nullable", true)
      ])
    ])
  };
  const defaults = { dot: { listen: "", cert_file: "", key_file: "" }, doh: { listen: "", cert_file: "", key_file: "" }, doq: { listen: "", cert_file: "", key_file: "" }, doh3: { listen: "", cert_file: "", key_file: "" } };
  const get = (object, path) => path.split(".").reduce((value, key) => value?.[key], object);
  function put(object, path, value) {
    const keys = path.split(".");
    const last = keys.pop();
    for (const key of keys) object = object[key] ||= {};
    object[last] = value;
  }
  function diff(before, after) {
    const result = {};
    for (const [key, value] of Object.entries(after)) {
      if (JSON.stringify(before?.[key]) === JSON.stringify(value)) continue;
      result[key] = value !== null && typeof value === "object" && !Array.isArray(value) && before?.[key] !== null && typeof before?.[key] === "object" ? diff(before[key], value) : value;
    }
    return result;
  }
  function valueOf(input, descriptor) {
    if (descriptor.path === "filter_file" || descriptor.path?.endsWith("_file")) return descriptor.type === "nullable" && input.value === "" ? null : input.value;
    if (descriptor.type === "checkbox") return input.checked;
    if (descriptor.type === "lines") return input.value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    if (descriptor.type === "nullable") return input.value.trim() || null;
    if (descriptor.type === "number") {
      if (!input.value.trim() || !Number.isSafeInteger(Number(input.value))) throw new I.MessageError("settings.error.integer", () => ({ label: descriptor.label }));
      return Number(input.value);
    }
    return input.value.trim();
  }
  function create(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }
  function translated(tag, className, key) {
    const node = create(tag, className);
    I.bind(node, key);
    return node;
  }
  function render(container, settings, onChange) {
    container.replaceChildren();
    for (const [page, descriptor] of Object.entries(pages)) {
      const section = create("section"); section.id = `settings-${page}`; section.hidden = true;
      for (const item of descriptor.groups) {
        const card = create("section", "panel settings-card"); card.append(translated("h2", "", item.titleKey), translated("p", "muted small", item.helpKey));
        const fieldset = create("fieldset", "settings-fields");
        const legend = translated("legend", "sr-only", item.titleKey); fieldset.append(legend);
        if (item.optional) {
          const label = create("label", "toggle-label");
          const enabled = create("input"); enabled.type = "checkbox"; enabled.id = `enable-${item.optional}`; enabled.checked = settings[item.optional] !== null;
          label.append(enabled, translated("span", "", item.enableKey)); card.append(label);
          fieldset.disabled = !enabled.checked;
          enabled.addEventListener("change", () => { fieldset.disabled = !enabled.checked; onChange(); });
        }
        for (const descriptor of item.fields) {
          const wrap = create("div", descriptor.type === "lines" ? "field wide" : "field");
          const id = `setting-${descriptor.path.replaceAll(".", "-")}`;
          const input = create(descriptor.type === "lines" ? "textarea" : descriptor.type === "select" ? "select" : "input"); input.id = id; input.dataset.path = descriptor.path;
          if (!["lines", "select"].includes(descriptor.type)) input.type = descriptor.type === "checkbox" || descriptor.type === "number" ? descriptor.type : "text";
          for (const [value, labelKey] of descriptor.options || []) { const option = translated("option", "", labelKey); option.value = value; input.append(option); }
          input.spellcheck = false; input.autocomplete = "off";
          if (descriptor.type === "lines") input.rows = 4;
          if (descriptor.min !== undefined) input.min = String(descriptor.min);
          if (descriptor.max !== undefined) input.max = String(descriptor.max);
          if (descriptor.type === "number") input.step = "1";
          if (!["checkbox", "nullable", "lines"].includes(descriptor.type) || descriptor.path === "upstreams.servers") input.required = true;
          const value = get(settings, descriptor.path) ?? get(defaults, descriptor.path);
          if (descriptor.type === "checkbox") input.checked = Boolean(value);
          else input.value = Array.isArray(value) ? value.join("\n") : value ?? "";
          input.dataset.initialValue = descriptor.type === "checkbox" ? String(input.checked) : input.value;
          const label = create("label", descriptor.type === "checkbox" ? "toggle-label" : ""); label.htmlFor = id;
          if (descriptor.type === "checkbox") { label.append(input, translated("span", "", descriptor.labelKey)); wrap.append(label); }
          else { I.bind(label, descriptor.labelKey); wrap.append(label, input); }
          if (descriptor.helpKey) { const help = translated("small", "", descriptor.helpKey); help.id = `${id}-help`; input.setAttribute("aria-describedby", help.id); wrap.append(help); }
          input.addEventListener("input", () => { updateFilterSource(container); onChange(); });
          fieldset.append(wrap);
        }
        card.append(fieldset); section.append(card);
      }
      container.append(section);
    }
    updateFilterSource(container);
  }
  function updateFilterSource(container) {
    const file = container.querySelector('[data-path="filter_file"]');
    for (const input of container.querySelectorAll('[data-path^="filter."]')) input.disabled = Boolean(file?.value);
  }
  function read(container, base, validate = true) {
    const result = structuredClone(base);
    for (const descriptor of Object.values(pages).flatMap((page) => page.groups)) {
      const newlyEnabled = descriptor.optional && base[descriptor.optional] === null;
      if (descriptor.optional) {
        if (!container.querySelector(`#enable-${descriptor.optional}`).checked) { result[descriptor.optional] = null; continue; }
        if (!result[descriptor.optional]) result[descriptor.optional] = structuredClone(defaults[descriptor.optional]);
      }
      for (const entry of descriptor.fields) {
        const input = container.querySelector(`[data-path="${entry.path}"]`);
        if (input.disabled) continue;
        const displayed = entry.type === "checkbox" ? String(input.checked) : input.value;
        if (!newlyEnabled && displayed === input.dataset.initialValue) continue;
        if (validate && !input.checkValidity()) {
          const validity = input.validity || {};
          const advice = entry.type === "number" && (validity.badInput || validity.stepMismatch) ? "integer"
            : validity.valueMissing ? "required"
            : entry.type === "number" && (validity.rangeUnderflow || validity.rangeOverflow) ? "range" : "check";
          throw new I.MessageError("settings.error.invalid", () => ({
            page: pages[Object.keys(pages).find((page) => pages[page].groups.includes(descriptor))].title,
            label: entry.label,
            message: t(`settings.validation.${advice}`, { min: entry.min, max: entry.max })
          }));
        }
        put(result, entry.path, valueOf(input, entry));
      }
    }
    return result;
  }
  // Only edit known template keys in their owning tables; never interpolate TOML syntax.
  function networkTemplate(template, listen, upstreams) {
    const lines = template.split("\n");
    const changes = new Map([["listen", listen], ...Object.entries(upstreams).map(([key, value]) => [`upstreams.${key}`, value])]);
    let table = "";
    for (let index = 0; index < lines.length; index += 1) {
      const header = /^\s*\[([^\]]+)\]\s*$/.exec(lines[index]);
      if (header) { table = header[1]; continue; }
      const match = /^\s*(\w+)\s*=/.exec(lines[index]);
      const path = match && (table ? `${table}.${match[1]}` : match[1]);
      if (changes.has(path)) { lines[index] = `${match[1]} = ${JSON.stringify(changes.get(path))}`; changes.delete(path); }
    }
    if (changes.size) throw new I.MessageError("app.templateInvalid", { key: [...changes.keys()].join(", ") });
    return lines.join("\n");
  }
  return { pages, defaults, get, put, diff, valueOf, render, read, updateFilterSource, networkTemplate };
})();

"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const I = PariI18n, S = PariSettings, C = PariCharts;
  const message = (key, params) => ({ key, params });
  const display = (value) => value?.key ? I.t(value.key, value.params) : I.messages[value] ? I.t(value) : String(value ?? "");
  function text(id, value) {
    const node = $(id);
    if (value?.key) I.bind(node, value.key, value.params);
    else if (I.messages[value]) I.bind(node, value);
    else { I.clear(node); node.textContent = value?.message || value || ""; }
  }
  let lastStatus = null, lastStatusTime = null;
  const K = PariCache;
  let logPage = null, logRevision = null, logFilter = { search: "", status: null };
  let readCacheRules = () => [], cacheStatus = null, cacheInspection = null, inspectedQuery = null;
  const state = { template: "", step: 0, original: "", revision: null, backup: false, dirty: false, busy: false, polling: false, settings: null, formDirty: false, settingsStale: false, view: "overview", samples: [], hours: 1, statsUpdated: 0, pendingFocus: null };
  const session = PariSession.createClient({ onUnauthorized: () => { clearStatistics(); page("login"); } });
  const api = session.request;
  const panels = ["loading", "setup", "login", "overview", "logs", "config"];

  function focusAfterAction(id) {
    if (state.busy) state.pendingFocus = id;
    else $(id).focus();
  }

  function changeSession(token) { session.setToken(token); clearStatistics(); }

  function notice(message, error = false) {
    text("notice", message);
    $("notice").classList.toggle("error", error);
    $("notice").hidden = !message;
  }

  function page(name, focus = true) {
    for (const panel of panels) $(`${panel}-panel`).hidden = panel !== name;
    const authenticated = ["overview", "logs", "config"].includes(name);
    $("navigation").hidden = !authenticated;
    $("logout").hidden = !authenticated;
    for (const view of ["overview", "logs", ...Object.keys(S.pages), "advanced"]) {
      const active = ["overview", "logs"].includes(name) ? view === name : name === "config" && view === state.view;
      $(`nav-${view}`).classList.toggle("active", active);
      if (active) $(`nav-${view}`).setAttribute("aria-current", "page");
      else $(`nav-${view}`).removeAttribute("aria-current");
    }
    text("page-context", message("app.context", () => ({ page: display({ loading: "app.connecting", setup: "app.setup", login: "app.login", overview: "app.overview", logs: "app.logs", config: S.pages[state.view]?.title || "app.advanced" }[name]) })));
    if (focus) focusAfterAction("main");
  }

  function updateEditorState() {
    state.dirty = state.formDirty || $("config-toml").value !== state.original;
    text("edit-state", state.dirty ? "app.dirty" : "app.saved");
    $("save-config").disabled = state.busy || state.revision === null;
    text("save-config", state.dirty ? "app.save" : "app.reapply");
    $("rollback-config").disabled = state.busy || !state.backup;
    text("config-revision", state.revision === null ? "" : message("app.revision", { revision: state.revision }));
  }

  async function action(work) {
    if (state.busy) return;
    state.busy = true;
    $("main").inert = true;
    $("navigation").inert = true;
    $("main").setAttribute("aria-busy", "true");
    notice("app.working");
    try { await work(); } catch (error) { if (!PariSession.isStale(error)) notice(error.key ? error : error.message || "app.offline", true); }
    finally {
      $("main").inert = false;
      $("navigation").inert = false;
      state.busy = false;
      $("main").removeAttribute("aria-busy");
      updateEditorState();
      if (state.pendingFocus && !$("confirm-dialog").open) $(state.pendingFocus).focus();
      state.pendingFocus = null;
    }
  }

  function confirmAction(title, description, label, work) {
    text("confirm-title", title);
    text("confirm-description", description);
    text("confirm-action", label);
    const dialog = $("confirm-dialog");
    dialog.returnValue = "cancel";
    dialog.addEventListener("close", () => { if (dialog.returnValue === "confirm") void action(work); }, { once: true });
    dialog.showModal();
  }

  async function start() {
    page("loading", false);
    $("loading-panel").setAttribute("aria-busy", "true");
    $("retry-start").hidden = true;
    try {
      const session = await api("session");
      if (session.setup_required) {
        state.template = (await api("template")).toml;
        page("setup");
      } else page("login");
      notice("");
    } catch (error) {
      notice(error.key ? error : error.message || "app.startFailed", true);
      $("retry-start").hidden = false;
    } finally { $("loading-panel").setAttribute("aria-busy", "false"); }
  }

  function showStep(step) {
    state.step = step;
    for (let index = 0; index < 3; index += 1) {
      $(`setup-step-${index}`).hidden = index !== step;
      for (const input of $(`setup-step-${index}`).querySelectorAll("input, textarea")) input.disabled = index !== step;
      if (index === step) $(`step-label-${index}`).setAttribute("aria-current", "step");
      else $(`step-label-${index}`).removeAttribute("aria-current");
    }
    $("setup-back").hidden = step === 0;
    $("setup-next").hidden = step === 2;
    $("setup-submit").hidden = step !== 2;
    $(`setup-step-${step}`).querySelector("input, textarea")?.focus();
  }

  function socketAddress(value) {
    const match = /^(?:\[([0-9a-fA-F:.]+)\]|(\d{1,3}(?:\.\d{1,3}){3})):(\d{1,5})$/.exec(value);
    return match && Number(match[3]) > 0 && Number(match[3]) <= 65535 && (match[1] ? match[1].includes(":") : match[2].split(".").every((part) => Number(part) <= 255));
  }

  // Only replace uniquely identified root keys before the first active TOML table.
  function networkTemplate(template, listen, upstream) {
    const lines = template.split("\n");
    const section = lines.findIndex((line) => /^\s*\[/.test(line));
    const end = section < 0 ? lines.length : section;
    for (const [key, value] of [["listen", listen], ["upstream", upstream]]) {
      const matches = [];
      for (let index = 0; index < end; index += 1) if (new RegExp(`^\\s*${key}\\s*=`).test(lines[index])) matches.push(index);
      if (matches.length !== 1) throw new I.MessageError("app.templateInvalid", { key });
      lines[matches[0]] = `${key} = ${JSON.stringify(value)}`;
    }
    return lines.join("\n");
  }

  $("setup-next").addEventListener("click", () => {
    notice("");
    if (!$("setup-form").reportValidity()) return;
    if (state.step === 0 && $("setup-password").value !== $("setup-confirm").value) {
      notice("app.passwordMismatch", true); $("setup-confirm").focus(); return;
    }
    if (state.step === 1) {
      const listen = $("setup-listen").value.trim();
      const upstream = $("setup-upstream").value.trim();
      if (!socketAddress(listen) || !socketAddress(upstream)) { notice("app.addressFormat", true); return; }
      if (listen === upstream) { notice("app.selfUpstream", true); return; }
      try { $("setup-toml").value = networkTemplate(state.template, listen, upstream); }
      catch (error) { notice(error, true); return; }
    }
    showStep(state.step + 1);
  });
  $("setup-back").addEventListener("click", () => { notice(""); showStep(state.step - 1); });
  $("setup-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (state.step !== 2) { $("setup-next").click(); return; }
    void action(async () => {
      const result = await api("setup", "POST", { username: $("setup-username").value.trim(), password: $("setup-password").value, toml: $("setup-toml").value }, { "X-PariNS-Setup": $("setup-token").value.trim() });
      changeSession(result.token);
      for (const id of ["setup-token", "setup-password", "setup-confirm"]) $(id).value = "";
      await enterConsole();
      notice("app.setupSaved");
    });
  });

  async function loadConfig() {
    const config = await api("config");
    const parsed = await api("config/parse", "POST", { toml: config.toml });
    state.original = config.toml;
    state.revision = config.revision;
    state.backup = config.has_backup;
    $("config-toml").value = config.toml;
    installSettings(parsed.settings);
    $("diff-panel").hidden = true;
    updateEditorState();
  }

  function installSettings(settings) {
    state.settings = settings; state.formDirty = false; state.settingsStale = false;
    const changed = () => {
      state.formDirty = true;
      $("diff-panel").hidden = true;
      updateEditorState();
    };
    S.render($("settings-forms"), settings, changed);
    readCacheRules = K.rules($("cache-rules"), settings.cache.rules || [], changed);
    displaySettingsPage();
  }

  function displaySettingsPage() {
    const advanced = state.view === "advanced";
    for (const name of Object.keys(S.pages)) if ($(`settings-${name}`)) $(`settings-${name}`).hidden = name !== state.view;
    $("advanced-editor").hidden = !advanced;
    $("form-help").hidden = advanced;
    $("management-tls-help").hidden = state.view !== "security";
    $("certificate-import-panel").hidden = state.view !== "security";
    $("cache-tools").hidden = state.view !== "cache";
    $("cache-rules-panel").hidden = state.view !== "cache";
    text("config-title", S.pages[state.view]?.titleKey || "app.advanced");
    text("config-intro", S.pages[state.view]?.introKey || "app.advancedIntro");
  }

  async function syncDraft() {
    if (!state.formDirty) return;
    const next = S.read($("settings-forms"), state.settings);
    next.cache.rules = readCacheRules();
    const changed = S.diff(state.settings, next);
    if (Object.keys(changed).length) {
      const result = await api("config/preview", "POST", { toml: $("config-toml").value, changes: changed });
      $("config-toml").value = result.toml;
      installSettings(result.settings);
    } else state.formDirty = false;
    updateEditorState();
  }

  async function navigate(view) {
    if (view === "logs") { state.view = view; page("logs"); await loadLogs(); return; }
    if (view === "overview") { state.view = view; page("overview"); if (state.statsUpdated) C.trend($("activity-chart"), state.samples, state.hours); return; }
    if (view === "advanced") await syncDraft();
    else if (state.settingsStale) {
      const parsed = await api("config/parse", "POST", { toml: $("config-toml").value });
      installSettings(parsed.settings);
    }
    state.view = view; displaySettingsPage(); page("config");
  }

  function unavailable() {
    lastStatus = null; lastStatusTime = null;
    cacheStatus = null; cacheInspection = null; inspectedQuery = null;
    K.renderStats($("cache-stats"), null, null);
    $("cache-clear-all").disabled = true; $("cache-clear-name").disabled = true;
    I.clear($("cache-inspection")); $("cache-inspection").replaceChildren();
    text("service-badge", "app.disconnected"); $("service-badge").classList.add("stopped");
    text("service-title", "app.statusUnavailable");
    text("service-detail", "app.connectionHelp");
    for (const id of ["metric-requests", "metric-blocked", "metric-cache", "metric-latency", "metric-inflight", "metric-failures", "status-listen", "status-uptime"]) text(id, "—");
    $("activity-chart").replaceChildren();
    const note = document.createElement("p"); note.className = "chart-empty"; I.bind(note, "app.statsUnavailable"); $("activity-chart").append(note);
    C.table($("response-chart"), [], true); C.table($("latency-chart"), [], true);
    state.statsUpdated = 0;
  }

  function clearStatistics() {
    logPage = null; logRevision = null;
    $("logs-results").replaceChildren(); text("logs-summary", "app.logsUnread");
    $("logs-next").disabled = true; $("logs-clear").disabled = true;
    $("certificate-pem").value = ""; $("private-key-pem").value = "";
    state.samples = []; state.statsUpdated = 0;
    unavailable();
    text("status-revision", "—");
    text("last-updated", "app.notUpdated");
    text("service-error", ""); $("service-error").hidden = true;
  }

  async function status() {
    const owner = session.snapshot();
    try {
      const data = await api("status");
      session.ensureCurrent(owner);
      let samples = state.samples;
      const refreshStats = Date.now() - state.statsUpdated >= 60000;
      if (refreshStats) samples = (await api("stats")).samples;
      session.ensureCurrent(owner);
      // Render only after all awaited work, while this session still owns both results.
      state.samples = samples;
      if (refreshStats) state.statsUpdated = Date.now();
      renderStatus(data);
    } catch (error) { session.ensureCurrent(owner); throw error; }
  }

  function renderStatus(data) {
    lastStatus = data; lastStatusTime = Date.now();
    cacheStatus = data.running ? data : null;
    K.renderStats($("cache-stats"), data.running ? data.cache : null, data.refresh);
    $("cache-clear-all").disabled = !cacheStatus?.cache;
    if (cacheInspection && (cacheInspection.revision !== data.revision || cacheInspection.epoch !== data.cache?.epoch)) {
      cacheInspection = null; inspectedQuery = null; $("cache-clear-name").disabled = true;
      text("cache-inspection", "app.cacheChanged");
    }
    text("service-badge", data.running ? "app.running" : "app.stopped");
    $("service-badge").classList.toggle("stopped", !data.running);
    text("service-title", data.running ? "app.started" : "app.notStarted");
    text("service-detail", data.running ? "app.runningHelp" : "app.stoppedHelp");
    text("status-revision", String(data.revision));
    text("service-error", data.last_error ? message("api.INVALID_CONFIG", { detail: data.last_error }) : "");
    $("service-error").hidden = !data.last_error;
    const counters = data.running ? data.metrics?.counters : undefined;
    const format = C.format;
    text("metric-requests", format(counters?.requests));
    text("metric-blocked", counters ? format(counters.query_blocked + counters.response_blocked) : "—");
    text("metric-inflight", data.running ? format(data.metrics?.request_inflight) : "—");
    text("metric-failures", format(counters?.upstream_failures));
    const lookups = (counters?.cache_hits || 0) + (counters?.cache_misses || 0);
    text("metric-cache", lookups ? `${((counters.cache_hits / lookups) * 100).toFixed(1)}%` : "—");
    const latency = data.running ? data.metrics?.request_latency : null;
    text("metric-latency", latency?.count ? (latency.sum_micros / latency.count / 1000).toFixed(2) : "—");
    text("status-listen", data.listen || "app.notConfigured");
    const uptime = data.uptime_seconds;
    text("status-uptime", data.running && Number.isFinite(uptime) ? message("app.uptime", { days: Math.floor(uptime / 86400), hours: Math.floor(uptime / 3600) % 24, minutes: Math.floor(uptime / 60) % 60 }) : "—");
    renderDistributions(data);
    text("last-updated", message("app.value", () => ({ value: I.date(lastStatusTime, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) })));
    C.trend($("activity-chart"), state.samples, state.hours);
  }

  function renderDistributions(data) {
    const counters = data.running ? data.metrics?.counters : undefined;
    const latency = data.running ? data.metrics?.request_latency : null;
    C.table($("response-chart"), [["responses_noerror", "app.rcodeSuccess"], ["responses_nxdomain", "app.rcodeMissing"], ["responses_servfail", "app.rcodeFailed"], ["responses_refused", "app.rcodeRefused"], ["responses_other", "app.rcodeOther"]].map(([key, label]) => ({ label: I.t(label), count: counters?.[key] || 0 })), !counters);
    C.table($("latency-chart"), latency ? C.histogram(latency.buckets) : [], !latency);
  }

  async function enterConsole() {
    // Preserve an unsaved draft across re-authentication; never replace it from polling.
    if (!state.dirty) await loadConfig();
    page("overview");
    try { await status(); } catch (error) { if (!PariSession.isStale(error)) unavailable(); throw error; }
  }

  $("login-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void action(async () => {
      const result = await api("login", "POST", { username: $("login-username").value.trim(), password: $("login-password").value });
      changeSession(result.token);
      $("login-password").value = "";
      await enterConsole();
      notice(state.dirty ? "app.resumedDraft" : "");
    });
  });

  function preview() {
    const oldLines = state.original.split("\n");
    const newLines = $("config-toml").value.split("\n");
    let prefix = 0;
    while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) prefix += 1;
    let oldEnd = oldLines.length, newEnd = newLines.length;
    while (oldEnd > prefix && newEnd > prefix && oldLines[oldEnd - 1] === newLines[newEnd - 1]) { oldEnd -= 1; newEnd -= 1; }
    const changedLines = [...oldLines.slice(prefix, oldEnd).map((line) => `− ${line}`), ...newLines.slice(prefix, newEnd).map((line) => `+ ${line}`)];
    text("config-diff", state.dirty ? message("app.value", () => ({ value: [I.t("app.diffStart", { line: prefix + 1 }), ...changedLines].join("\n") })) : "app.noChanges");
    $("diff-panel").hidden = false;
    focusAfterAction("diff-title");
  }

  $("config-toml").addEventListener("input", () => { state.settingsStale = true; updateEditorState(); $("diff-panel").hidden = true; });
  $("preview-config").addEventListener("click", () => void action(async () => { await syncDraft(); preview(); notice("app.previewReady"); }));
  $("validate-config").addEventListener("click", () => void action(async () => { await syncDraft(); const result = await api("config/validate", "POST", { toml: $("config-toml").value }); notice(result.restart_required ? "app.validRestart" : "app.validCache"); }));
  $("reload-config").addEventListener("click", () => {
    const reload = async () => { await loadConfig(); notice("app.reloaded"); };
    if (state.dirty) confirmAction("app.discardTitle", "app.discardHelp", "app.discard", reload);
    else void action(reload);
  });
  $("save-config").addEventListener("click", () => {
    void action(async () => {
      await syncDraft(); preview(); notice("");
      const impact = await api("config/validate", "POST", { toml: $("config-toml").value });
    confirmAction(impact.restart_required ? "app.saveRestartTitle" : "app.saveCacheTitle", impact.restart_required ? "app.saveRestartHelp" : "app.saveCacheHelp", "app.save", async () => {
      const toml = $("config-toml").value;
      await api("config", "PUT", { toml, revision: state.revision });
      await loadConfig();
      await status();
      notice("app.configSaved");
    });
    });
  });
  $("rollback-config").addEventListener("click", () => confirmAction("app.rollbackTitle", "app.rollbackHelp", "app.rollback", async () => {
    await api("config/rollback", "POST", { revision: state.revision });
    await loadConfig();
    await status();
    notice("app.restored");
  }));
  $("export-config").addEventListener("click", () => void action(async () => {
    await syncDraft();
    const url = URL.createObjectURL(new Blob([$("config-toml").value], { type: "text/plain;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url; link.download = "parins.toml"; document.body.append(link); link.click(); link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    notice("app.exported");
  }));
  $("cache-inspect-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void action(async () => {
      cacheInspection = null; inspectedQuery = null; $("cache-clear-name").disabled = true;
      text("cache-inspection", "app.inspecting");
      const query = { name: $("cache-name").value.trim(), qtype: $("cache-qtype").value.trim(), subnet: $("cache-subnet").value.trim() || null, edns: $("cache-edns").checked, dnssec_ok: $("cache-do").checked, checking_disabled: $("cache-cd").checked, recursion_desired: $("cache-rd").checked };
      try {
        const result = await api("cache/inspect", "POST", query);
        cacheInspection = result; inspectedQuery = query; I.clear($("cache-inspection")); K.renderInspection($("cache-inspection"), result);
        $("cache-clear-name").disabled = false;
        notice("app.inspected");
      } catch (error) { if (!PariSession.isStale(error)) text("cache-inspection", "app.inspectFailed"); throw error; }
    });
  });
  async function invalidateCache(input) {
    const result = await api("cache/invalidate", "POST", input);
    cacheInspection = null; inspectedQuery = null; $("cache-clear-name").disabled = true;
    text("cache-inspection", "app.cacheCleared");
    await status(); notice(message("app.removed", () => ({ count: I.number(result.removed) })));
  }
  $("cache-clear-all").addEventListener("click", () => {
    if (!cacheStatus?.cache) return;
    const input = { all: true, revision: cacheStatus.revision, epoch: cacheStatus.cache.epoch };
    confirmAction("app.clearCacheTitle", "app.clearCacheHelp", "app.clearCache", () => invalidateCache(input));
  });
  $("cache-clear-name").addEventListener("click", () => {
    if (!cacheInspection || !inspectedQuery) return;
    const input = { name: inspectedQuery.name, qtype: inspectedQuery.qtype, scope: $("cache-clear-scope").value.trim() || null, revision: cacheInspection.revision, epoch: cacheInspection.epoch };
    confirmAction("app.clearSelectedTitle", message("app.clearScopeHelp", () => ({ name: input.name, qtype: input.qtype, scope: input.scope || I.t("app.allScopes") })), "app.clearSelected", () => invalidateCache(input));
  });
  $("logout").addEventListener("click", () => {
    const logout = async () => {
      try { await api("logout", "POST", {}); }
      finally {
        changeSession(null); state.original = ""; state.revision = null; state.backup = false; state.settings = null; state.formDirty = false; state.settingsStale = false;
        $("settings-forms").replaceChildren();
        $("cache-rules").replaceChildren(); readCacheRules = () => [];
        $("config-toml").value = ""; text("config-diff", ""); updateEditorState();
        page("login");
      }
      notice("app.signedOut");
    };
    if (state.dirty) confirmAction("app.logoutTitle", "app.logoutHelp", "app.logout", logout);
    else void action(logout);
  });
  async function loadLogs(before = null) {
    text("logs-summary", "app.reading");
    $("logs-next").disabled = true;
    try {
      const result = await api("query-log/list", "POST", { ...logFilter, before_id: before, limit: 50 });
      logPage = result.page; logRevision = result.revision;
      PariQueryLog.render($("logs-results"), logPage);
      const updated = Date.now(), page = logPage;
      text("logs-summary", page.enabled ? message("app.logSummary", () => ({ count: I.number(page.entries.length), total: I.number(page.total), time: I.date(updated, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) })) : "app.logsDisabled");
      $("logs-next").disabled = !logPage.next_cursor;
      $("logs-clear").disabled = !logPage.total;
    } catch (error) {
      if (!PariSession.isStale(error)) { logPage = null; logRevision = null; $("logs-results").replaceChildren(); $("logs-clear").disabled = true; text("logs-summary", "app.loadFailed"); }
      throw error;
    }
  }
  $("logs-filter").addEventListener("submit", (event) => {
    event.preventDefault(); logFilter = { search: $("logs-search").value.trim(), status: $("logs-status").value || null };
    void action(async () => { await loadLogs(); notice(""); });
  });
  $("logs-refresh").addEventListener("click", () => void action(async () => { await loadLogs(); notice(""); }));
  $("logs-next").addEventListener("click", () => { if (logPage?.next_cursor) void action(async () => { await loadLogs(logPage.next_cursor); notice(""); }); });
  $("logs-settings").addEventListener("click", () => void action(async () => { await navigate("runtime"); focusAfterAction("setting-query_log-enabled"); notice("app.logsSettingsHelp"); }));
  $("logs-clear").addEventListener("click", () => {
    if (logRevision === null) return;
    const revision = logRevision;
    confirmAction("app.clearLogsTitle", "app.clearLogsHelp", "app.clearLogs", async () => {
      await api("query-log/clear", "POST", { revision }); await loadLogs(); notice("app.logsCleared");
    });
  });
  $("certificate-import-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void action(async () => {
      const target = $("certificate-target").value;
      if (!$(`enable-${target}`).checked) throw new I.MessageError("app.enableListener");
      const certificate_pem = $("certificate-pem").value, private_key_pem = $("private-key-pem").value;
      // Import before validating empty certificate paths; preserve every other draft control.
      const result = await api("certificates/import", "POST", { revision: state.revision, certificate_pem, private_key_pem });
      $("certificate-pem").value = ""; $("private-key-pem").value = "";
      $(`setting-${target}-cert_file`).value = result.identity.cert_file;
      $(`setting-${target}-key_file`).value = result.identity.key_file;
      state.formDirty = true; updateEditorState();
      notice("app.certificateImported");
    });
  });
  for (const name of ["overview", "logs", ...Object.keys(S.pages), "advanced"]) $(`nav-${name}`).addEventListener("click", () => void action(async () => { await navigate(name); notice(""); }));
  $("open-config").addEventListener("click", () => void action(async () => { await navigate("dns"); notice(""); }));
  $("refresh-status").addEventListener("click", () => void action(async () => { state.statsUpdated = 0; try { await status(); notice(""); } catch (error) { if (!PariSession.isStale(error)) unavailable(); throw error; } }));
  for (const button of document.querySelectorAll("[data-hours]")) button.addEventListener("click", () => {
    state.hours = Number(button.dataset.hours);
    for (const control of document.querySelectorAll("[data-hours]")) { control.setAttribute("aria-pressed", String(control === button)); control.classList.toggle("active", control === button); }
    if (state.statsUpdated) C.trend($("activity-chart"), state.samples, state.hours);
  });
  $("retry-start").addEventListener("click", () => void start());
  window.addEventListener("beforeunload", (event) => { if (state.dirty) { event.preventDefault(); event.returnValue = ""; } });
  window.addEventListener("resize", () => { if (state.statsUpdated && !$("overview-panel").hidden) C.trend($("activity-chart"), state.samples, state.hours); });
  setInterval(async () => {
    if (!session.token || state.busy || state.polling || document.hidden) return;
    state.polling = true;
    try { await status(); } catch (error) { if (!PariSession.isStale(error)) { unavailable(); notice(message("app.statusFailed", () => ({ error: error.key ? display(error) : error.message })), true); } }
    finally { state.polling = false; }
  }, 5000);
  $("language-select").value = I.locale;
  $("language-select").addEventListener("change", (event) => I.setLocale(event.target.value));
  window.addEventListener("parins:language", () => {
    $("language-select").value = I.locale;
    if (lastStatus) renderDistributions(lastStatus);
    if (state.statsUpdated) C.trend($("activity-chart"), state.samples, state.hours);
  });
  I.apply();
  showStep(0);
  void start();
})();

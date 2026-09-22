"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const S = PariSettings, C = PariCharts;
  const K = PariCache;
  let readCacheRules = () => [], cacheStatus = null, cacheInspection = null, inspectedQuery = null;
  const state = { template: "", step: 0, original: "", revision: null, backup: false, dirty: false, busy: false, polling: false, settings: null, formDirty: false, settingsStale: false, view: "overview", samples: [], hours: 1, statsUpdated: 0, pendingFocus: null };
  const session = PariSession.createClient({ onUnauthorized: () => { clearStatistics(); page("login"); } });
  const api = session.request;
  const panels = ["loading", "setup", "login", "overview", "config"];

  function focusAfterAction(id) {
    if (state.busy) state.pendingFocus = id;
    else $(id).focus();
  }

  function changeSession(token) { session.setToken(token); clearStatistics(); }

  function notice(message, error = false) {
    $("notice").textContent = message;
    $("notice").classList.toggle("error", error);
    $("notice").hidden = !message;
  }

  function page(name, focus = true) {
    for (const panel of panels) $(`${panel}-panel`).hidden = panel !== name;
    const authenticated = name === "overview" || name === "config";
    $("navigation").hidden = !authenticated;
    $("logout").hidden = !authenticated;
    for (const view of ["overview", ...Object.keys(S.pages), "advanced"]) {
      const active = name === "overview" ? view === "overview" : name === "config" && view === state.view;
      $(`nav-${view}`).classList.toggle("active", active);
      if (active) $(`nav-${view}`).setAttribute("aria-current", "page");
      else $(`nav-${view}`).removeAttribute("aria-current");
    }
    $("page-context").textContent = `管理台 / ${{ loading: "连接服务", setup: "首次初始化", login: "登录", overview: "仪表盘", config: S.pages[state.view]?.title || "高级配置" }[name]}`;
    if (focus) focusAfterAction("main");
  }

  function updateEditorState() {
    state.dirty = state.formDirty || $("config-toml").value !== state.original;
    $("edit-state").textContent = state.dirty ? "有未保存修改" : "未修改";
    $("save-config").disabled = state.busy || state.revision === null;
    $("save-config").textContent = state.dirty ? "保存并应用" : "重新应用当前配置";
    $("rollback-config").disabled = state.busy || !state.backup;
    $("config-revision").textContent = state.revision === null ? "" : `· 版本 ${state.revision}`;
  }

  async function action(work) {
    if (state.busy) return;
    state.busy = true;
    $("main").inert = true;
    $("navigation").inert = true;
    $("main").setAttribute("aria-busy", "true");
    notice("正在处理，请稍候…");
    try { await work(); } catch (error) { if (!PariSession.isStale(error)) notice(error.message || "无法连接管理服务，请稍后重试。", true); }
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
    $("confirm-title").textContent = title;
    $("confirm-description").textContent = description;
    $("confirm-action").textContent = label;
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
      notice(error.message || "无法连接管理服务。请确认 PariNS 正在运行。", true);
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
      if (matches.length !== 1) throw new Error(`配置模板的顶层 ${key} 不唯一，无法安全生成配置。`);
      lines[matches[0]] = `${key} = ${JSON.stringify(value)}`;
    }
    return lines.join("\n");
  }

  $("setup-next").addEventListener("click", () => {
    notice("");
    if (!$("setup-form").reportValidity()) return;
    if (state.step === 0 && $("setup-password").value !== $("setup-confirm").value) {
      notice("两次输入的密码不一致。", true); $("setup-confirm").focus(); return;
    }
    if (state.step === 1) {
      const listen = $("setup-listen").value.trim();
      const upstream = $("setup-upstream").value.trim();
      if (!socketAddress(listen) || !socketAddress(upstream)) { notice("监听地址和上游地址必须使用 IP:端口；IPv6 地址请加方括号。", true); return; }
      if (listen === upstream) { notice("上游不能指向自己的监听地址。", true); return; }
      try { $("setup-toml").value = networkTemplate(state.template, listen, upstream); }
      catch (error) { notice(error.message, true); return; }
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
      notice("初始化已保存。请在运行概览确认 DNS 状态；系统 DNS 设置未修改。");
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
    $("cache-tools").hidden = state.view !== "cache";
    $("cache-rules-panel").hidden = state.view !== "cache";
    $("config-title").textContent = S.pages[state.view]?.title || "高级配置";
    $("config-intro").textContent = S.pages[state.view]?.intro || "直接编辑完整 TOML。服务端负责解析与校验，未保存内容只留在当前页面。";
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
    if (view === "overview") { state.view = view; page("overview"); if (state.statsUpdated) C.trend($("activity-chart"), state.samples, state.hours); return; }
    if (view === "advanced") await syncDraft();
    else if (state.settingsStale) {
      const parsed = await api("config/parse", "POST", { toml: $("config-toml").value });
      installSettings(parsed.settings);
    }
    state.view = view; displaySettingsPage(); page("config");
  }

  function unavailable() {
    cacheStatus = null; cacheInspection = null; inspectedQuery = null;
    K.renderStats($("cache-stats"), null, null);
    $("cache-clear-all").disabled = true; $("cache-clear-name").disabled = true;
    $("cache-inspection").replaceChildren();
    $("service-badge").textContent = "连接中断"; $("service-badge").classList.add("stopped");
    $("service-title").textContent = "无法获取最新状态";
    $("service-detail").textContent = "请检查管理服务与网络连接。历史草稿仍保留，旧统计不会显示为在线状态。";
    for (const id of ["metric-requests", "metric-blocked", "metric-cache", "metric-latency", "metric-inflight", "metric-failures", "status-listen", "status-uptime"]) $(id).textContent = "—";
    $("activity-chart").replaceChildren();
    const note = document.createElement("p"); note.className = "chart-empty"; note.textContent = "统计暂时不可用，恢复连接后自动更新。"; $("activity-chart").append(note);
    C.table($("response-chart"), [], true); C.table($("latency-chart"), [], true);
    state.statsUpdated = 0;
  }

  function clearStatistics() {
    state.samples = []; state.statsUpdated = 0;
    unavailable();
    $("status-revision").textContent = "—";
    $("last-updated").textContent = "尚未刷新";
    $("service-error").textContent = ""; $("service-error").hidden = true;
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
    cacheStatus = data.running ? data : null;
    K.renderStats($("cache-stats"), data.running ? data.cache : null, data.refresh);
    $("cache-clear-all").disabled = !cacheStatus?.cache;
    if (cacheInspection && (cacheInspection.revision !== data.revision || cacheInspection.epoch !== data.cache?.epoch)) {
      cacheInspection = null; inspectedQuery = null; $("cache-clear-name").disabled = true;
      $("cache-inspection").textContent = "缓存策略或失效代次已改变，请重新检查。";
    }
    $("service-badge").textContent = data.running ? "正在运行" : "未运行";
    $("service-badge").classList.toggle("stopped", !data.running);
    $("service-title").textContent = data.running ? "DNS 服务已启动" : "DNS 服务尚未就绪";
    $("service-detail").textContent = data.running ? "服务正在接受请求。运行状态不代表上游网络一定可达。" : "请查看下方错误或检查配置；管理台仍可用于修正设置。";
    $("status-revision").textContent = String(data.revision);
    $("service-error").textContent = data.last_error || "";
    $("service-error").hidden = !data.last_error;
    const counters = data.running ? data.metrics?.counters : undefined;
    const format = C.format;
    $("metric-requests").textContent = format(counters?.requests);
    $("metric-blocked").textContent = counters ? format(counters.query_blocked + counters.response_blocked) : "—";
    $("metric-inflight").textContent = data.running ? format(data.metrics?.request_inflight) : "—";
    $("metric-failures").textContent = format(counters?.upstream_failures);
    const lookups = (counters?.cache_hits || 0) + (counters?.cache_misses || 0);
    $("metric-cache").textContent = lookups ? `${((counters.cache_hits / lookups) * 100).toFixed(1)}%` : "—";
    const latency = data.running ? data.metrics?.request_latency : null;
    $("metric-latency").textContent = latency?.count ? (latency.sum_micros / latency.count / 1000).toFixed(2) : "—";
    $("status-listen").textContent = data.listen || "未配置";
    const uptime = data.uptime_seconds;
    $("status-uptime").textContent = data.running && Number.isFinite(uptime) ? `${Math.floor(uptime / 86400)} 天 ${Math.floor(uptime / 3600) % 24} 小时 ${Math.floor(uptime / 60) % 60} 分` : "—";
    C.table($("response-chart"), [["responses_noerror", "NOERROR · 成功"], ["responses_nxdomain", "NXDOMAIN · 不存在"], ["responses_servfail", "SERVFAIL · 失败"], ["responses_refused", "REFUSED · 拒绝"], ["responses_other", "其他响应"]].map(([key, label]) => ({ label, count: counters?.[key] || 0 })), !counters);
    C.table($("latency-chart"), latency ? C.histogram(latency.buckets) : [], !latency);
    $("last-updated").textContent = new Date().toLocaleTimeString("zh-CN");
    C.trend($("activity-chart"), state.samples, state.hours);
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
      notice(state.dirty ? "登录成功。此前未保存的编辑仍在配置管理中，请确认版本后再应用。" : "");
    });
  });

  function preview() {
    const oldLines = state.original.split("\n");
    const newLines = $("config-toml").value.split("\n");
    let prefix = 0;
    while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) prefix += 1;
    let oldEnd = oldLines.length, newEnd = newLines.length;
    while (oldEnd > prefix && newEnd > prefix && oldLines[oldEnd - 1] === newLines[newEnd - 1]) { oldEnd -= 1; newEnd -= 1; }
    $("config-diff").textContent = state.dirty ? [`@@ 从第 ${prefix + 1} 行开始 @@`, ...oldLines.slice(prefix, oldEnd).map((line) => `− ${line}`), ...newLines.slice(prefix, newEnd).map((line) => `+ ${line}`)].join("\n") : "没有变更。";
    $("diff-panel").hidden = false;
    focusAfterAction("diff-title");
  }

  $("config-toml").addEventListener("input", () => { state.settingsStale = true; updateEditorState(); $("diff-panel").hidden = true; });
  $("preview-config").addEventListener("click", () => void action(async () => { await syncDraft(); preview(); notice("变更预览已生成，尚未保存。"); }));
  $("validate-config").addEventListener("click", () => void action(async () => { await syncDraft(); const result = await api("config/validate", "POST", { toml: $("config-toml").value }); notice(result.restart_required ? "校验通过。保存将重启 DNS 并清空缓存。" : "校验通过。保存仅切换缓存策略并清空缓存，不重启 DNS 监听。"); }));
  $("reload-config").addEventListener("click", () => {
    const reload = async () => { await loadConfig(); notice("已读取最新保存配置。"); };
    if (state.dirty) confirmAction("放弃未保存修改？", "重新加载会覆盖当前编辑内容。如需保留，请取消并先导出。", "放弃并重新加载", reload);
    else void action(reload);
  });
  $("save-config").addEventListener("click", () => {
    void action(async () => {
      await syncDraft(); preview(); notice("");
      const impact = await api("config/validate", "POST", { toml: $("config-toml").value });
    confirmAction(impact.restart_required ? "保存并重启 DNS？" : "保存缓存策略？", impact.restart_required ? "配置将短暂重启 DNS，正在处理的请求可能中断，缓存将清空；旧配置保留用于回滚。" : "DNS 监听保持运行，旧缓存与后台刷新将失效；新请求使用新策略，旧配置保留用于回滚。", "保存并应用", async () => {
      const toml = $("config-toml").value;
      await api("config", "PUT", { toml, revision: state.revision });
      await loadConfig();
      await status();
      notice("配置已保存。请检查运行概览，确认 DNS 已成功启动。");
    });
    });
  });
  $("rollback-config").addEventListener("click", () => confirmAction("回滚上一份配置？", "上一份配置将替换当前配置，缓存将清空；涉及非缓存设置时重启 DNS。未保存的编辑会被丢弃。", "确认回滚", async () => {
    await api("config/rollback", "POST", { revision: state.revision });
    await loadConfig();
    await status();
    notice("已恢复上一份配置。请检查运行概览确认启动结果。");
  }));
  $("export-config").addEventListener("click", () => void action(async () => {
    await syncDraft();
    const url = URL.createObjectURL(new Blob([$("config-toml").value], { type: "text/plain;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url; link.download = "parins.toml"; document.body.append(link); link.click(); link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    notice("已导出当前编辑内容，不包含管理账户或会话令牌。请妥善保管配置中的地址和路径。");
  }));
  $("cache-inspect-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void action(async () => {
      cacheInspection = null; inspectedQuery = null; $("cache-clear-name").disabled = true;
      $("cache-inspection").textContent = "正在检查…";
      const query = { name: $("cache-name").value.trim(), qtype: $("cache-qtype").value.trim(), subnet: $("cache-subnet").value.trim() || null, edns: $("cache-edns").checked, dnssec_ok: $("cache-do").checked, checking_disabled: $("cache-cd").checked, recursion_desired: $("cache-rd").checked };
      try {
        const result = await api("cache/inspect", "POST", query);
        cacheInspection = result; inspectedQuery = query; K.renderInspection($("cache-inspection"), result);
        $("cache-clear-name").disabled = false;
        notice("检查完成；使用的是已保存配置，不是尚未保存的草稿。");
      } catch (error) { if (!PariSession.isStale(error)) $("cache-inspection").textContent = "检查未完成，请确认查询条件后重试。"; throw error; }
    });
  });
  async function invalidateCache(input) {
    const result = await api("cache/invalidate", "POST", input);
    cacheInspection = null; inspectedQuery = null; $("cache-clear-name").disabled = true;
    $("cache-inspection").textContent = "清理完成。新查询仍可重新填充缓存。";
    await status(); notice(`已移除 ${result.removed} 个缓存变体。`);
  }
  $("cache-clear-all").addEventListener("click", () => {
    if (!cacheStatus?.cache) return;
    const input = { all: true, revision: cacheStatus.revision, epoch: cacheStatus.cache.epoch };
    confirmAction("清空全部缓存？", "将移除正向、否定及过期结果。旧的进行中请求不能回填；后续请求可能产生更多回源流量。DNS 监听不会停止。", "清空缓存", () => invalidateCache(input));
  });
  $("cache-clear-name").addEventListener("click", () => {
    if (!cacheInspection || !inspectedQuery) return;
    const input = { name: inspectedQuery.name, qtype: inspectedQuery.qtype, scope: $("cache-clear-scope").value.trim() || null, revision: cacheInspection.revision, epoch: cacheInspection.epoch };
    confirmAction("清理选定缓存？", `${input.name} / ${input.qtype} / ${input.scope || "所有 ECS 范围"}。包含此条件下所有 EDNS/DO/CD/RD 标志变体；其他域名保留。`, "确认清理", () => invalidateCache(input));
  });
  $("logout").addEventListener("click", () => {
    const logout = async () => {
      try { await api("logout", "POST", {}); }
      finally {
        changeSession(null); state.original = ""; state.revision = null; state.backup = false; state.settings = null; state.formDirty = false; state.settingsStale = false;
        $("settings-forms").replaceChildren();
        $("cache-rules").replaceChildren(); readCacheRules = () => [];
        $("config-toml").value = ""; $("config-diff").textContent = ""; updateEditorState();
        page("login");
      }
      notice("已退出登录。");
    };
    if (state.dirty) confirmAction("退出并放弃修改？", "你有未保存的配置修改。退出后会清除当前草稿，请先导出需要保留的内容。", "退出登录", logout);
    else void action(logout);
  });
  for (const name of ["overview", ...Object.keys(S.pages), "advanced"]) $(`nav-${name}`).addEventListener("click", () => void action(async () => { await navigate(name); notice(""); }));
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
    try { await status(); } catch (error) { if (!PariSession.isStale(error)) { unavailable(); notice(`状态刷新失败：${error.message}`, true); } }
    finally { state.polling = false; }
  }, 5000);
  showStep(0);
  void start();
})();

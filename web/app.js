"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const state = { token: null, template: "", step: 0, original: "", revision: null, backup: false, dirty: false, busy: false, polling: false };
  const panels = ["loading", "setup", "login", "overview", "config"];

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
    for (const view of ["overview", "config"]) {
      $(`nav-${view}`).classList.toggle("active", view === name);
      if (view === name) $(`nav-${view}`).setAttribute("aria-current", "page");
      else $(`nav-${view}`).removeAttribute("aria-current");
    }
    $("page-context").textContent = `管理台 / ${{ loading: "连接服务", setup: "首次初始化", login: "登录", overview: "运行概览", config: "配置管理" }[name]}`;
    if (focus) $("main").focus();
  }

  async function api(path, method = "GET", body, extraHeaders = {}) {
    const headers = { ...extraHeaders };
    if (state.token) headers.Authorization = `Bearer ${state.token}`;
    if (body !== undefined) headers["Content-Type"] = "application/json";
    const response = await fetch(`/api/${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), credentials: "omit", cache: "no-store", redirect: "error" });
    const data = await response.json();
    if (!response.ok) {
      if (response.status === 401 && state.token) {
        state.token = null;
        page("login");
      }
      const message = response.status === 409 ? "配置版本已变化。请先导出未保存内容，再重新加载最新配置后合并修改。" : data.error?.message || `请求失败（HTTP ${response.status}）`;
      throw new Error(message);
    }
    return data;
  }

  function updateEditorState() {
    state.dirty = $("config-toml").value !== state.original;
    $("edit-state").textContent = state.dirty ? "有未保存修改" : "未修改";
    $("save-config").disabled = state.busy || state.revision === null;
    $("save-config").textContent = state.dirty ? "保存并应用" : "重新应用当前配置";
    $("rollback-config").disabled = state.busy || !state.backup;
    $("config-revision").textContent = state.revision === null ? "" : `· 版本 ${state.revision}`;
  }

  async function action(work) {
    if (state.busy) return;
    state.busy = true;
    const controls = [...document.querySelectorAll("button, input, textarea")];
    const previous = controls.map((control) => control.disabled);
    controls.forEach((control) => { control.disabled = true; });
    $("main").setAttribute("aria-busy", "true");
    notice("正在处理，请稍候…");
    try { await work(); } catch (error) { notice(error.message || "无法连接管理服务，请稍后重试。", true); }
    finally {
      controls.forEach((control, index) => { control.disabled = previous[index]; });
      state.busy = false;
      $("main").removeAttribute("aria-busy");
      updateEditorState();
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
      state.token = result.token;
      for (const id of ["setup-token", "setup-password", "setup-confirm"]) $(id).value = "";
      await enterConsole();
      notice("初始化已保存。请在运行概览确认 DNS 状态；系统 DNS 设置未修改。");
    });
  });

  async function loadConfig() {
    const config = await api("config");
    state.original = config.toml;
    state.revision = config.revision;
    state.backup = config.has_backup;
    $("config-toml").value = config.toml;
    $("diff-panel").hidden = true;
    updateEditorState();
  }

  async function status() {
    const data = await api("status");
    $("service-badge").textContent = data.running ? "正在运行" : "未运行";
    $("service-badge").classList.toggle("stopped", !data.running);
    $("service-title").textContent = data.running ? "DNS 服务已启动" : "DNS 服务尚未就绪";
    $("service-detail").textContent = data.running ? "服务正在接受请求。运行状态不代表上游网络一定可达。" : "请查看下方错误或检查配置；管理台仍可用于修正设置。";
    $("status-revision").textContent = String(data.revision);
    $("service-error").textContent = data.last_error || "";
    $("service-error").hidden = !data.last_error;
    const counters = data.metrics?.counters;
    const format = (value) => value === undefined ? "—" : new Intl.NumberFormat("zh-CN").format(value);
    $("metric-requests").textContent = format(counters?.requests);
    $("metric-inflight").textContent = format(data.metrics?.request_inflight);
    $("metric-failures").textContent = format(counters?.upstream_failures);
    const lookups = (counters?.cache_hits || 0) + (counters?.cache_misses || 0);
    $("metric-cache").textContent = lookups ? `${((counters.cache_hits / lookups) * 100).toFixed(1)}%` : "—";
    $("last-updated").textContent = new Date().toLocaleTimeString("zh-CN");
  }

  async function enterConsole() {
    // Preserve an unsaved draft across re-authentication; never replace it from polling.
    if (!state.dirty) await loadConfig();
    page("overview");
    await status();
  }

  $("login-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void action(async () => {
      const result = await api("login", "POST", { username: $("login-username").value.trim(), password: $("login-password").value });
      state.token = result.token;
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
    $("diff-title").focus();
  }

  $("config-toml").addEventListener("input", () => { updateEditorState(); $("diff-panel").hidden = true; });
  $("preview-config").addEventListener("click", preview);
  $("validate-config").addEventListener("click", () => void action(async () => { await api("config/validate", "POST", { toml: $("config-toml").value }); notice("配置校验通过，尚未应用。保存会重启 DNS 实例并清空内存缓存。"); }));
  $("reload-config").addEventListener("click", () => {
    const reload = async () => { await loadConfig(); notice("已读取最新保存配置。"); };
    if (state.dirty) confirmAction("放弃未保存修改？", "重新加载会覆盖当前编辑内容。如需保留，请取消并先导出。", "放弃并重新加载", reload);
    else void action(reload);
  });
  $("save-config").addEventListener("click", () => {
    preview();
    confirmAction("保存并重启 DNS？", "配置会在校验后保存，并短暂重启 DNS 实例。正在处理的请求可能中断，内存缓存将清空；旧配置将保留一份用于回滚。", "保存并应用", async () => {
      const toml = $("config-toml").value;
      await api("config", "PUT", { toml, revision: state.revision });
      await loadConfig();
      await status();
      notice("配置已保存。请检查运行概览，确认 DNS 已成功启动。");
    });
  });
  $("rollback-config").addEventListener("click", () => confirmAction("回滚上一份配置？", "上一份配置将替换当前配置并重启 DNS 实例，内存缓存将清空。未保存的编辑会被丢弃。", "确认回滚", async () => {
    await api("config/rollback", "POST", { revision: state.revision });
    await loadConfig();
    await status();
    notice("已恢复上一份配置。请检查运行概览确认启动结果。");
  }));
  $("export-config").addEventListener("click", () => {
    const url = URL.createObjectURL(new Blob([$("config-toml").value], { type: "text/plain;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url; link.download = "parins.toml"; document.body.append(link); link.click(); link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    notice("已导出当前编辑内容，不包含管理账户或会话令牌。请妥善保管配置中的地址和路径。");
  });
  $("logout").addEventListener("click", () => {
    const logout = async () => {
      try { await api("logout", "POST", {}); }
      finally {
        state.token = null; state.original = ""; state.revision = null; state.backup = false;
        $("config-toml").value = ""; $("config-diff").textContent = ""; updateEditorState();
        page("login");
      }
      notice("已退出登录。");
    };
    if (state.dirty) confirmAction("退出并放弃修改？", "你有未保存的配置修改。退出后会清除当前草稿，请先导出需要保留的内容。", "退出登录", logout);
    else void action(logout);
  });
  for (const id of ["nav-config", "open-config"]) $(id).addEventListener("click", () => page("config"));
  $("nav-overview").addEventListener("click", () => page("overview"));
  $("refresh-status").addEventListener("click", () => void action(async () => { await status(); notice(""); }));
  $("retry-start").addEventListener("click", () => void start());
  window.addEventListener("beforeunload", (event) => { if (state.dirty) { event.preventDefault(); event.returnValue = ""; } });
  setInterval(async () => {
    if (!state.token || state.busy || state.polling || document.hidden) return;
    state.polling = true;
    try { await status(); } catch (error) { notice(`状态刷新失败：${error.message}`, true); }
    finally { state.polling = false; }
  }, 5000);
  showStep(0);
  void start();
})();

"use strict";

// Form metadata is presentation only. Rust remains the parser and validator.
globalThis.PariSettings = (() => {
  const field = (path, label, type = "text", help = "", min, max) => ({ path, label, type, help, min, max });
  const number = (path, label, min, max, help = "") => field(path, label, "number", help, min, max);
  const select = (path, label, options) => ({ ...field(path, label, "select"), options });
  const group = (title, help, fields, optional) => ({ title, help, fields, optional });
  const pages = {
    dns: { title: "DNS 设置", intro: "选择请求入口，多行填写上游，并选择负载均衡或并发查询。IPv6 地址请加方括号。", groups: [
      group("多上游解析", "每行一个端点：IP[:端口]、udp://、tcp://、tls://、https://主机/dns-query 或 quic://。行尾可加 weight=2 设置权重。组内上游应有一致的解析、过滤及 ECS 策略；并发模式会把查询发给多个服务商。启用后需关闭旧版副本、上游 TLS 和 TLS 连接池。", [field("upstreams.servers", "上游 DNS（每行一个）", "lines", "例如 udp://192.0.2.53:53 weight=2；加密端点必须通过证书验证。"), select("upstreams.mode", "请求模式", [["weighted", "加权负载均衡 · 按配置权重轮流查询"], ["parallel", "并发请求 · 优先返回可用响应"]]), field("upstreams.bootstrap", "引导 DNS（每行一个 IP:端口）", "lines", "使用域名形式的上游时必填。只用于解析上游主机名；不会使用系统 DNS，也不替你选择第三方。"), number("upstreams.max_parallel", "单次并发上游上限", 1, 32, "并发模式下不能小于上游数量。"), number("upstreams.max_extra_inflight", "全局额外上游并发预算", 1, 65536, "预算耗尽时减少并发，仍遵守查询总超时。"), field("upstreams.ca_file", "自定义上游 CA 文件（可选）", "nullable")], "upstreams"),
      group("监听与解析", "普通 DNS 同时提供 UDP 和 TCP。不会修改操作系统的 DNS 设置，也不会替你选择公共上游。", [field("listen", "DNS 监听地址", "text", "本机示例 127.0.0.1:5353；局域网可绑定指定网卡地址。开放到外部前限制访问来源。"), field("upstream", "旧版单上游 DNS 地址", "text", "仅在未启用多上游时使用。输入 IP:端口；开启旧版上游 TLS 时用于 DoT。不要指向自身。"), number("query_timeout_ms", "查询总超时（毫秒）", 1, 60000), number("tcp_io_timeout_ms", "TCP 读写超时（毫秒）", 1, 60000)]),
      group("等价上游副本", "仅用于解析策略一致的两个副本：主请求超过等待时间后，才并发请求副本。不是主备切换或多上游负载均衡。TLS 模式下，两副本共享证书服务器名。", [field("scheduler.secondary", "副本地址", "text", "与主上游不同的 IP:端口。"), number("scheduler.hedge_after_ms", "发起副本查询前等待（毫秒）", 1, 60000), number("scheduler.max_extra_inflight", "副本额外并发上限", 1, 65536)], "scheduler")
    ] },
    cache: { title: "缓存与 ECS", intro: "复用有效解析结果，控制内存占用，并明确子网信息的使用边界。", groups: [
      group("响应缓存", "缓存遵守 DNS TTL；ECS 子网变体独立隔离。仅修改缓存配置时无需重启监听，但会清空旧策略缓存。", [field("cache.enabled", "启用缓存", "checkbox"), number("cache.max_entries", "缓存条目上限", 1, 262144), number("cache.max_bytes", "缓存记账字节预算", 512, 1073741824, "8 MiB = 8388608 字节。包含条目元数据估算，不是进程 RSS 硬上限。"), number("cache.max_variants", "每个查询的子网变体上限", 1, 256), number("cache.shards", "并发分片数", 1, 64, "分片各自使用预算；更多分片不一定更快，也可能降低空间利用率。"), number("cache.negative_percent", "否定缓存预算比例（%）", 0, 90, "隔离 NXDOMAIN/NODATA，避免随机不存在域名挤出正向缓存。"), number("cache.max_ttl_secs", "正向缓存 TTL 上限（秒）", 1, 86400), number("cache.negative_ttl_cap_secs", "否定缓存 TTL 上限（秒）", 1, 86400)]),
      group("热门记录预取", "默认关闭。在热门条目接近过期时后台回源，当前请求仍返回有效缓存。并发、速率和失败退避共同限制额外流量。", [field("cache.prefetch.enabled", "启用预取", "checkbox"), number("cache.prefetch.min_hits", "触发所需命中次数", 1, 1000000), number("cache.prefetch.remaining_percent", "剩余 TTL 比例（%）", 1, 90), number("cache.prefetch.max_inflight", "后台刷新并发上限", 1, 256), number("cache.prefetch.rate_per_sec", "后台刷新每秒预算", 1, 10000), number("cache.prefetch.backoff_secs", "失败退避（秒）", 1, 3600)]),
      group("上游故障时使用过期结果", "默认关闭。先正常回源，失败后才可使用保留的正向结果；可能返回旧地址。否定答案和隐私降级结果不参与过期兜底。", [field("cache.stale.enabled", "启用故障兜底", "checkbox"), number("cache.stale.retention_secs", "过期后保留时长（秒）", 1, 604800), number("cache.stale.reply_ttl_secs", "过期应答 TTL（秒）", 1, 300)]),
      group("EDNS Client Subnet", "ECS 可能向上游披露客户端子网。默认关闭；启用后按前缀裁剪，ECS、无 ECS 和降级结果不会混用缓存。", [field("ecs.enabled", "启用 ECS", "checkbox"), number("ecs.ipv4_prefix", "IPv4 最大前缀长度", 0, 32, "数值越小，透露的地址信息越少。"), number("ecs.ipv6_prefix", "IPv6 最大前缀长度", 0, 128)])
    ] },
    filters: { title: "过滤规则", intro: "按域名精确匹配或后缀匹配；允许规则优先于拦截规则。不支持广告列表语法或订阅下载。", groups: [
      group("规则来源", "可在下方直接维护规则，或读取服务器上的规则文件。文件路径不是上传入口；相对路径基于服务状态目录。", [field("filter_file", "外部规则文件（留空使用下方规则）", "nullable", "文件存在时，以文件内的 enabled 和规则为准，下方内置规则不生效。清空路径才会切换回内置规则。")]),
      group("内置域名规则", "每行一个域名，不要填写 URL、通配符或 Adblock 表达式。后缀规则匹配该域名及其子域名。", [field("filter.enabled", "启用内置过滤", "checkbox"), field("filter.block_exact", "精确拦截", "lines", "例如 ads.example.com"), field("filter.block_suffix", "后缀拦截", "lines", "例如 example.com"), field("filter.allow_exact", "精确允许", "lines"), field("filter.allow_suffix", "后缀允许", "lines")])
    ] },
    security: { title: "安全与加密", intro: "为 DNS 传输配置加密。可粘贴 PEM 证书和私钥，或使用服务器已有文件。DNS 证书与管理台 HTTPS 相互独立。", groups: [
      group("上游 TLS（DoT）", "启用后，主上游和副本使用 DNS-over-TLS。必须提供匹配证书的服务器名；CA 留空使用内置可信根证书集。", [field("upstream_tls.server_name", "证书服务器名", "text", "例如 dns.example.com；不是连接 IP，也不是 URL。"), field("upstream_tls.ca_file", "自定义 CA 文件（可选）", "nullable")], "upstream_tls"),
      ...[["dot", "DNS-over-TLS", "TCP，通常使用 853 端口。"], ["doh", "DNS-over-HTTPS", "TCP，查询路径 /dns-query。"], ["doq", "DNS-over-QUIC", "UDP，通常使用 853 端口。"], ["doh3", "DNS-over-HTTP/3", "UDP，查询路径 /dns-query。"]].map(([key, title, help]) => group(title, `${help} 可用上方粘贴入口导入证书，自动填入路径；或填写服务器现有 PEM 文件路径。`, [field(`${key}.listen`, "监听地址"), field(`${key}.cert_file`, "证书 PEM 路径"), field(`${key}.key_file`, "私钥 PEM 路径")], key))
    ] },
    runtime: { title: "运行参数", intro: "为服务器设置有界的并发和资源预算。所有更改会在保存并应用后生效。", groups: [
      group("查询日志", "默认关闭。启用后在内存中记录客户端 IP、查询域名、答案和实际处理路径，只有登录的管理员可查看。达到容量或保留期限会删除；重启清空，不写入磁盘或聚合指标。", [field("query_log.enabled", "启用逐请求日志", "checkbox"), number("query_log.max_entries", "最多保留条数", 1, 10000), number("query_log.retention_secs", "保留时长（秒）", 1, 604800)]),
      group("全局并发", "限制整个 DNS 实例的资源使用。来源预算单独约束每个客户端子网，但不能代替防火墙访问控制。", [number("max_inflight", "请求并发上限", 1, 65536), number("max_tcp_connections", "TCP 连接上限", 1, 65536), number("shutdown_grace_ms", "平滑停止等待（毫秒）", 1, 60000)]),
      group("来源预算", "按传输连接的真实对端子网分组，不按客户端声明的 ECS 分组。", [field("source_limits.enabled", "启用来源预算", "checkbox"), number("source_limits.rate_per_sec", "每个来源每秒查询预算", 1, 1000000), number("source_limits.burst", "每个来源突发预算", 1, 1000000), number("source_limits.max_sources", "来源表容量", 1, 65536), number("source_limits.ipv4_prefix", "IPv4 分组前缀", 0, 32), number("source_limits.ipv6_prefix", "IPv6 分组前缀", 0, 128), number("source_limits.max_inflight", "每个来源的请求并发", 1, 65536), number("source_limits.max_connections", "每个来源的连接上限", 1, 65536)]),
      group("同类请求合并", "短时间内相同查询共享进行中的上游操作。各请求仍保留独立的响应和时限。", [field("coalescing.enabled", "启用请求合并", "checkbox"), number("coalescing.max_groups", "合并组上限", 1, 65536), number("coalescing.max_waiters", "每组合并等待者上限", 1, 65536)]),
      group("上游 TLS 连接池", "仅在启用上游 TLS 后可用。连接总量覆盖主上游和副本。", [field("upstream_pool.enabled", "启用连接复用", "checkbox"), number("upstream_pool.max_connections", "连接池容量", 1, 256), number("upstream_pool.idle_timeout_ms", "空闲连接超时（毫秒）", 1, 600000)]),
      group("观测接口", "仪表盘统计始终在内存中收集。stderr 聚合输出与只读观测接口是另外两个入口。", [number("metrics.interval_secs", "stderr 统计输出间隔（秒）", 0, 3600, "0 表示不定时输出，不影响管理台统计。"), field("admin_listen", "只读观测接口地址（可选）", "nullable", "只允许回环地址，例如 127.0.0.1:9090；留空关闭。不是管理台的监听地址。")])
    ] }
  };
  const defaults = { scheduler: { secondary: "", hedge_after_ms: 100, max_extra_inflight: 32 }, upstream_tls: { server_name: "", ca_file: null }, dot: { listen: "", cert_file: "", key_file: "" }, doh: { listen: "", cert_file: "", key_file: "" }, doq: { listen: "", cert_file: "", key_file: "" }, doh3: { listen: "", cert_file: "", key_file: "" } };
  const get = (object, path) => path.split(".").reduce((value, key) => value?.[key], object);
  defaults.upstreams = { servers: [], mode: "weighted", bootstrap: [], max_parallel: 32, max_extra_inflight: 128, ca_file: null };
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
      if (!input.value.trim() || !Number.isSafeInteger(Number(input.value))) throw new Error(`${descriptor.label}必须填写整数。`);
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
  function render(container, settings, onChange) {
    container.replaceChildren();
    for (const [page, descriptor] of Object.entries(pages)) {
      const section = create("section"); section.id = `settings-${page}`; section.hidden = true;
      for (const item of descriptor.groups) {
        const card = create("section", "panel settings-card"); card.append(create("h2", "", item.title), create("p", "muted small", item.help));
        const fieldset = create("fieldset", "settings-fields");
        const legend = create("legend", "sr-only", item.title); fieldset.append(legend);
        if (item.optional) {
          const label = create("label", "toggle-label");
          const enabled = create("input"); enabled.type = "checkbox"; enabled.id = `enable-${item.optional}`; enabled.checked = settings[item.optional] !== null;
          label.append(enabled, document.createTextNode(`启用 ${item.title}`)); card.append(label);
          fieldset.disabled = !enabled.checked;
          enabled.addEventListener("change", () => { fieldset.disabled = !enabled.checked; onChange(); });
        }
        for (const descriptor of item.fields) {
          const wrap = create("div", descriptor.type === "lines" ? "field wide" : "field");
          const id = `setting-${descriptor.path.replaceAll(".", "-")}`;
          const input = create(descriptor.type === "lines" ? "textarea" : descriptor.type === "select" ? "select" : "input"); input.id = id; input.dataset.path = descriptor.path;
          if (!["lines", "select"].includes(descriptor.type)) input.type = descriptor.type === "checkbox" || descriptor.type === "number" ? descriptor.type : "text";
          for (const [value, label] of descriptor.options || []) { const option = create("option", "", label); option.value = value; input.append(option); }
          input.spellcheck = false; input.autocomplete = "off";
          if (descriptor.type === "lines") input.rows = 4;
          if (descriptor.min !== undefined) input.min = String(descriptor.min);
          if (descriptor.max !== undefined) input.max = String(descriptor.max);
          if (descriptor.type === "number") input.step = "1";
          if (!["checkbox", "nullable", "lines"].includes(descriptor.type)) input.required = true;
          const value = get(settings, descriptor.path) ?? get(defaults, descriptor.path);
          if (descriptor.type === "checkbox") input.checked = Boolean(value);
          else input.value = Array.isArray(value) ? value.join("\n") : value ?? "";
          input.dataset.initialValue = descriptor.type === "checkbox" ? String(input.checked) : input.value;
          const label = create("label", descriptor.type === "checkbox" ? "toggle-label" : "", descriptor.label); label.htmlFor = id;
          if (descriptor.type === "checkbox") { label.prepend(input); wrap.append(label); } else wrap.append(label, input);
          if (descriptor.help) { const help = create("small", "", descriptor.help); help.id = `${id}-help`; input.setAttribute("aria-describedby", help.id); wrap.append(help); }
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
        if (validate && !input.checkValidity()) throw new Error(`请检查「${pages[Object.keys(pages).find((page) => pages[page].groups.includes(descriptor))].title}」中的「${entry.label}」：${input.validationMessage}`);
        put(result, entry.path, valueOf(input, entry));
      }
    }
    return result;
  }
  return { pages, defaults, get, put, diff, valueOf, render, read, updateFilterSource };
})();

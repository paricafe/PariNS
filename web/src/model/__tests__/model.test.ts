import { describe, expect, it } from "vitest";
import {
  ModelError, cacheRuleDraft, convertFieldValue, decodeCacheRule, defaultCacheRule,
  diffSettings, fieldDisplayValue, getPath, joinListener, networkTemplate, setPath,
  settingPages, splitListener,
} from "../index";
import { presentIssue } from "../../i18n";

describe("configuration form model", () => {
  it("describes the settings paths and one DoH listener with an HTTP/3 switch", () => {
    expect(settingPages.dns.groups.find((group) => group.id === "upstreams")?.fields[0].path).toBe("upstreams.servers");
    expect(settingPages.cache.groups.flatMap((group) => group.fields)
      .find((field) => field.path === "cache.negative_percent")).toMatchObject({ min: 0, max: 90 });
    expect(settingPages.security.groups.map((group) => group.optional)).toEqual([undefined, "dot", "doh", "doq"]);
    expect(settingPages.security.groups[2].fields.at(-1)?.path).toBe('doh.http3');
    expect(settingPages.security.groups[0].fields[0].path).toBe("web.public_host");
    expect(settingPages.security.groups.slice(1).every((group) => group.fields[0].type === "endpoint")).toBe(true);
  });

  it("offers the resolution mode before upstreams and shows recursion limits only in recursive mode", () => {
    const ids = settingPages.dns.groups.map((group) => group.id);
    expect(ids.indexOf("resolution")).toBeLessThan(ids.indexOf("upstreams"));
    expect(ids.indexOf("recursion")).toBeLessThan(ids.indexOf("upstreams"));
    const mode = settingPages.dns.groups.find((group) => group.id === "resolution")!.fields[0];
    expect(mode).toMatchObject({ path: "resolution.mode", type: "select", options: [["forward", "settings.resolution.mode.forward"], ["recursive", "settings.resolution.mode.recursive"]] });
    expect(convertFieldValue(mode, "recursive")).toBe("recursive");
    expect(() => convertFieldValue(mode, "iterative")).toThrow(ModelError);
    const recursion = settingPages.dns.groups.find((group) => group.id === "recursion")!;
    expect(recursion.when).toEqual({ path: "resolution.mode", value: "recursive" });
    expect(recursion.fields.map(({ path, type, min, max, advanced }) => ({ path, type, min, max, advanced: advanced ?? false }))).toEqual([
      { path: "recursion.ipv6", type: "checkbox", min: undefined, max: undefined, advanced: false },
      { path: "recursion.max_outbound_per_second", type: "number", min: 1, max: 10000, advanced: false },
      { path: "recursion.max_outbound_inflight", type: "number", min: 1, max: 4096, advanced: true },
      { path: "recursion.max_queries_per_request", type: "number", min: 1, max: 1000, advanced: true },
      { path: "recursion.max_depth", type: "number", min: 4, max: 64, advanced: true },
      { path: "recursion.cache_entries", type: "number", min: 1024, max: 1048576, advanced: true },
    ]);
    const depth = recursion.fields.find((field) => field.path === "recursion.max_depth")!;
    for (const raw of ["", "3", "65", "2e1"]) expect(() => convertFieldValue(depth, raw)).toThrow(ModelError);
    expect(convertFieldValue(depth, " 24 ")).toBe(24);
  });

  it("offers the global block response after the local rules, with addresses only for a custom IP", () => {
    const ids = settingPages.filters.groups.map((group) => group.id);
    expect(ids.indexOf("blockResponse")).toBe(ids.indexOf("filterRules") + 1);
    const fields = settingPages.filters.groups.find((group) => group.id === "blockResponse")!.fields;
    expect(fields.map(({ path, type, min, max, when }) => ({ path, type, min, max, when }))).toEqual([
      { path: "filter.block_response", type: "select", min: undefined, max: undefined, when: undefined },
      { path: "filter.block_ipv4", type: "nullable", min: undefined, max: undefined, when: { path: "filter.block_response", value: "custom_ip" } },
      { path: "filter.block_ipv6", type: "nullable", min: undefined, max: undefined, when: { path: "filter.block_response", value: "custom_ip" } },
      { path: "filter.blocked_ttl_secs", type: "number", min: 1, max: 86400, when: undefined },
    ]);
    expect(fields[0].options?.map(([value]) => value)).toEqual(["nodata", "null_ip", "custom_ip", "nxdomain", "refused"]);
    expect(() => convertFieldValue(fields[0], "drop")).toThrow(ModelError);
    expect(convertFieldValue(fields[1], " ")).toBeNull();
    expect(convertFieldValue(fields[1], " 192.0.2.1 ")).toBe("192.0.2.1");
    expect(() => convertFieldValue(fields[3], "0")).toThrow(ModelError);
    // Rule lines stay raw; Rust parses the optional block response after the domain.
    const block = settingPages.filters.groups.find((group) => group.id === "filterRules")!.fields.find((field) => field.path === "filter.block_suffix")!;
    expect(convertFieldValue(block, " track.example nxdomain \npage.example 192.0.2.10 2001:db8::10\n")).toEqual(["track.example nxdomain", "page.example 192.0.2.10 2001:db8::10"]);
  });

  it("writes a touched nested path immutably and emits a sparse patch", () => {
    const base = { listen: "127.0.0.1:53", cache: { enabled: true, max_bytes: 1024 } };
    const changed = setPath(base, "cache.enabled", false);
    expect(base.cache.enabled).toBe(true);
    expect(getPath(changed, "cache.enabled")).toBe(false);
    expect(diffSettings(base, changed)).toEqual({ cache: { enabled: false } });
    expect(diffSettings(base, base)).toEqual({});
  });

  it("replaces arrays and optional sections without touching unrelated paths", () => {
    expect(diffSettings({ cache: { max_bytes: 1000, rules: [defaultCacheRule()] } },
      { cache: { max_bytes: 1000, rules: [] } })).toEqual({ cache: { rules: [] } });
    expect(diffSettings({ dot: { listen: "[::]:853", cert_file: " cert.pem " } }, { dot: null }))
      .toEqual({ dot: null });
  });

  it("keeps paths verbatim and distinguishes empty from whitespace", () => {
    const file = settingPages.filters.groups[0].fields[0];
    expect(convertFieldValue(file, " cert.pem ")).toBe(" cert.pem ");
    expect(convertFieldValue(file, " ")).toBe(" ");
    expect(convertFieldValue(file, "")).toBeNull();
  });

  it("keeps numeric edits raw until conversion, then rejects partial or out-of-range input", () => {
    const field = settingPages.dns.groups.find((group) => group.id === "listeners")!.fields[1];
    expect(fieldDisplayValue(1000, field)).toBe("1000");
    for (const raw of ["", "-", "1e", "1e3", "1.5", "0", "60001"]) {
      expect(() => convertFieldValue(field, raw)).toThrow(ModelError);
    }
    expect(convertFieldValue(field, " 001000 ")).toBe(1000);
    const nullable = settingPages.runtime.groups.at(-1)!.fields[1];
    expect(convertFieldValue(nullable, "")).toBeNull();
  });

  it("preserves plain and encrypted listen round-trips, including IPv6 and port zero", () => {
    for (const listen of ["0.0.0.0:853", "[::]:853", "[::1]:0", "[::ffff:192.0.2.1]:65535", "[fe80::1%3]:1"]) {
      const parts = splitListener(listen);
      expect(joinListener(parts.address, parts.port)).toBe(listen);
    }
    expect(splitListener("")).toEqual({ address: "", port: "" });
    expect(joinListener(" [2001:db8::1] ", " 00053 ")).toBe("[2001:db8::1]:53");
    const field = settingPages.security.groups[1].fields[0];
    expect(fieldDisplayValue("[2001:0db8:0:0::1]:00853", field))
      .toEqual({ address: "2001:0db8:0:0::1", port: "00853" });
  });

  it("reports listener draft errors without altering raw input", () => {
    const field = settingPages.security.groups[1].fields[0];
    const raw = { address: "::1", port: "1e3" };
    expect(() => convertFieldValue(field, raw)).toThrowError(expect.objectContaining({
      key: "settings.listener.port.invalid", path: "dot.listen.port",
    }));
    expect(raw.port).toBe("1e3");
    expect(() => joinListener("[::1]:853", "853")).toThrowError(expect.objectContaining({
      key: "settings.listener.address.invalid",
    }));
  });

  it("edits setup template keys in their original tables only", () => {
    const template = 'listen = "127.0.0.1:53"\n[upstreams]\nservers = ["udp://192.0.2.1"]\nmode = "weighted"';
    expect(networkTemplate(template, "[::1]:1053", { servers: ["tls://dns.example:853"], mode: "parallel" }))
      .toContain('servers = ["tls://dns.example:853"]');
    expect(() => networkTemplate(template, "[::1]:1053", { prefer_h3: true })).toThrow(ModelError);
  });
});

describe("cache rule editor model", () => {
  it("keeps inherit distinct from explicit false", () => {
    const draft = { ...cacheRuleDraft(defaultCacheRule()), name: " Example.test ", suffix: "true", qtype: " aaaa ",
      negative_ttl_cap_secs: "30", stale: "false" };
    expect(decodeCacheRule(draft)).toEqual({
      name: "Example.test", suffix: true, qtype: "AAAA", bypass: false,
      max_ttl_secs: null, negative_ttl_cap_secs: 30, prefetch: null, stale: false,
    });
    expect(draft.name).toBe(" Example.test ");
  });

  it("rejects invalid TTL and missing names at conversion", () => {
    for (const ttl of ["0", "-1", "1.5", "1e3", "NaN", "604801"]) {
      const draft = { ...cacheRuleDraft(defaultCacheRule()), name: "example.test", max_ttl_secs: ttl };
      expect(() => decodeCacheRule(draft)).toThrowError(expect.objectContaining({ key: "views.ttlError" }));
    }
    expect(() => decodeCacheRule(cacheRuleDraft(defaultCacheRule())))
      .toThrowError(expect.objectContaining({ key: "views.domainError" }));
  });
});

describe("unit field ranges", () => {
  const fieldFor = (path: string) => Object.values(settingPages).flatMap((page) => page.groups).flatMap((group) => group.fields).find((field) => field.path === path)!;
  const issue = (path: string, draft: string) => {
    try { convertFieldValue(fieldFor(path), draft); } catch (error) { return error as ModelError; }
    throw new Error("expected a range error");
  };

  it("reports unit field bounds in their largest exact unit, in both languages", () => {
    // 65 GB is above the 64 GiB snapshot limit.
    const bytes = issue("cache.persistence.max_bytes", String(65 * 1024 ** 3));
    expect(bytes.key).toBe("settings.validation.rangeUnit");
    expect(presentIssue(bytes, "zh-CN")).toBe("请填写 1 MB 到 64 GB 之间的值。");
    expect(presentIssue(bytes, "en")).toBe("Enter a value from 1 MB to 64 GB.");
    const seconds = issue("statistics.retention_secs", "60");
    expect(presentIssue(seconds, "zh-CN")).toBe("请填写 1 小时 到 365 天 之间的值。");
    expect(presentIssue(seconds, "en")).toBe("Enter a value from 1 hour to 365 days.");
  });

  it("keeps whole-number ranges for fields without units", () => {
    const plain = issue("cache.max_entries", "0");
    expect(plain.key).toBe("settings.validation.range");
    expect(presentIssue(plain, "en")).toBe("Enter a whole number from 1 to 16777216.");
  });
});

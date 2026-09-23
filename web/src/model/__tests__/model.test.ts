import { describe, expect, it } from "vitest";
import {
  ModelError, cacheRuleDraft, convertFieldValue, decodeCacheRule, defaultCacheRule,
  diffSettings, fieldDisplayValue, getPath, joinListener, networkTemplate, setPath,
  settingPages, splitListener,
} from "../index";

describe("configuration form model", () => {
  it("describes the existing settings paths and four encrypted listeners", () => {
    expect(settingPages.dns.groups[0].fields[0].path).toBe("upstreams.servers");
    expect(settingPages.cache.groups.flatMap((group) => group.fields)
      .find((field) => field.path === "cache.negative_percent")).toMatchObject({ min: 0, max: 90 });
    expect(settingPages.security.groups.map((group) => group.optional)).toEqual(["dot", "doh", "doq", "doh3"]);
    expect(settingPages.security.groups.every((group) => group.fields[0].type === "endpoint")).toBe(true);
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
    const field = settingPages.dns.groups[1].fields[1];
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
    const field = settingPages.security.groups[0].fields[0];
    expect(fieldDisplayValue("[2001:0db8:0:0::1]:00853", field))
      .toEqual({ address: "2001:0db8:0:0::1", port: "00853" });
  });

  it("reports listener draft errors without altering raw input", () => {
    const field = settingPages.security.groups[0].fields[0];
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
    for (const ttl of ["0", "-1", "1.5", "1e3", "NaN", "86401"]) {
      const draft = { ...cacheRuleDraft(defaultCacheRule()), name: "example.test", max_ttl_secs: ttl };
      expect(() => decodeCacheRule(draft)).toThrowError(expect.objectContaining({ key: "views.ttlError" }));
    }
    expect(() => decodeCacheRule(cacheRuleDraft(defaultCacheRule())))
      .toThrowError(expect.objectContaining({ key: "views.domainError" }));
  });
});

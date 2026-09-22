#!/usr/bin/env node
// Zero-dependency browser-independent tests for the embedded console contract.
import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";
import { fileURLToPath } from "node:url";
import path from "node:path";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
for (const file of ["settings.js", "charts.js", "session.js"]) vm.runInThisContext(fs.readFileSync(path.join(root, "web", file), "utf8"), { filename: file });
const S = globalThis.PariSettings, C = globalThis.PariCharts;
let passed = 0;
function test(title, run) { run(); passed += 1; console.log(`ok ${passed} - ${title}`); }

test("equal settings create no patch", () => assert.deepEqual(S.diff({ cache: { enabled: true }, filter: { block_exact: ["a.test"] } }, { cache: { enabled: true }, filter: { block_exact: ["a.test"] } }), {}));
test("nested patches only contain edited fields", () => assert.deepEqual(S.diff({ cache: { enabled: true, max_bytes: 8388608 }, listen: "127.0.0.1:53" }, { cache: { enabled: false, max_bytes: 8388608 }, listen: "127.0.0.1:53" }), { cache: { enabled: false } }));
test("optional sections are explicitly removed with null", () => assert.deepEqual(S.diff({ dot: { listen: "[::]:853", cert_file: "cert.pem", key_file: "key.pem" } }, { dot: null }), { dot: null }));
test("new optional sections retain the full required shape", () => assert.deepEqual(S.diff({ upstream_tls: null }, { upstream_tls: { server_name: "dns.example", ca_file: null } }), { upstream_tls: { server_name: "dns.example", ca_file: null } }));
test("existing optional sections patch only the changed path", () => assert.deepEqual(S.diff({ dot: { listen: "[::]:853", cert_file: "cert.pem", key_file: "key.pem" } }, { dot: { listen: "[::]:853", cert_file: "next.pem", key_file: "key.pem" } }), { dot: { cert_file: "next.pem" } }));
test("arrays are replacements, not object-index patches", () => assert.deepEqual(S.diff({ filter: { block_suffix: ["a.test", "b.test"] } }, { filter: { block_suffix: [] } }), { filter: { block_suffix: [] } }));
test("unchanged relative file paths are not rewritten", () => assert.deepEqual(S.diff({ filter_file: "rules/local.toml", max_inflight: 10 }, { filter_file: "rules/local.toml", max_inflight: 11 }), { max_inflight: 11 }));
test("file path values preserve intentional leading and trailing spaces", () => {
  for (const path of ["filter_file", "dot.cert_file", "dot.key_file", "upstream_tls.ca_file"]) assert.equal(S.valueOf({ value: " cert.pem " }, { path, type: "nullable" }), " cert.pem ");
  assert.equal(S.valueOf({ value: " " }, { path: "filter_file", type: "nullable" }), " ");
  assert.equal(S.valueOf({ value: "" }, { path: "filter_file", type: "nullable" }), null);
});
test("reading one dirty control never reparses or trims untouched paths", () => {
  const base = { max_inflight: 10, filter_file: " rules.toml ", dot: { listen: "127.0.0.1:853", cert_file: " cert.pem ", key_file: " key.pem " }, upstream_tls: { server_name: "dns.test", ca_file: " ca.pem " } };
  const controls = new Map();
  for (const group of Object.values(S.pages).flatMap((page) => page.groups)) {
    if (group.optional) { if (!(group.optional in base)) base[group.optional] = null; controls.set(`#enable-${group.optional}`, { checked: base[group.optional] !== null }); }
    for (const field of group.fields) {
      const value = S.get(base, field.path);
      const display = value === undefined || value === null ? "" : String(value);
      controls.set(`[data-path="${field.path}"]`, { value: display, checked: Boolean(value), dataset: { initialValue: field.type === "checkbox" ? String(Boolean(value)) : display }, checkValidity: () => true });
    }
  }
  controls.get('[data-path="max_inflight"]').value = "11";
  const result = S.read({ querySelector: (selector) => controls.get(selector) }, base);
  assert.deepEqual(S.diff(base, result), { max_inflight: 11 });
  assert.equal(result.dot.cert_file, " cert.pem ");
  assert.equal(result.filter_file, " rules.toml ");
  assert.equal(result.upstream_tls.ca_file, " ca.pem ");
});
test("nullable fields clear to null", () => assert.equal(S.valueOf({ value: "  " }, { type: "nullable" }), null));
test("domain lists preserve ordering and remove blank lines", () => assert.deepEqual(S.valueOf({ value: " a.test \r\n\n b.test\n" }, { type: "lines" }), ["a.test", "b.test"]));
test("checkboxes use checked, not their text value", () => assert.equal(S.valueOf({ checked: false, value: "on" }, { type: "checkbox" }), false));
test("numeric zero remains valid for prefix and metrics fields", () => assert.equal(S.valueOf({ value: "0" }, { type: "number" }), 0));
test("blank and fractional integer controls fail instead of coercing", () => { for (const value of ["", " ", "1.5", "NaN", "9007199254740993"]) assert.throws(() => S.valueOf({ value }, { type: "number", label: "数量" })); });
test("path access and writes preserve unrelated values", () => { const result = { cache: { enabled: true } }; S.put(result, "cache.max_entries", 12); assert.equal(S.get(result, "cache.max_entries"), 12); assert.equal(result.cache.enabled, true); });
test("every Config root key has a visual control", () => {
  const roots = new Set(Object.values(S.pages).flatMap((page) => page.groups.flatMap((group) => group.fields.map((field) => field.path.split(".")[0]))));
  assert.deepEqual([...roots].sort(), ["listen", "upstream", "query_timeout_ms", "tcp_io_timeout_ms", "shutdown_grace_ms", "max_inflight", "max_tcp_connections", "source_limits", "ecs", "cache", "filter", "coalescing", "metrics", "dot", "doh", "doq", "doh3", "upstream_tls", "upstream_pool", "filter_file", "admin_listen", "scheduler"].sort());
});
test("all form paths are unique", () => { const paths = Object.values(S.pages).flatMap((page) => page.groups.flatMap((group) => group.fields.map((field) => field.path))); assert.equal(new Set(paths).size, paths.length); });

const now = 200000000;
const sample = (offset, values = {}) => ({ timestamp_ms: now + offset, elapsed_seconds: 60, running: true, generation: 1, requests: 120, cache_hits: 60, blocked: 0, ...values });
test("interval counters become per-second rates, not cumulative differences", () => assert.deepEqual(C.series([sample(-120000), sample(-60000, { requests: 30 })], "requests", 1, now).map((point) => point.value), [2, 0.5]));
test("partial sample uses its actual elapsed time", () => assert.equal(C.series([sample(-1000, { requests: 10, elapsed_seconds: 2 })], "requests", 1, now)[0].value, 5));
test("generation changes break paths without losing valid interval count", () => { const points = C.series([sample(-120000), sample(-60000, { generation: 2 })], "requests", 1, now); assert.equal(points[1].breakBefore, true); assert.equal(points[1].value, 2); });
test("stopped and zero-duration intervals remain gaps", () => assert.deepEqual(C.series([sample(-120000, { running: false }), sample(-60000, { elapsed_seconds: 0 })], "requests", 1, now).map((point) => point.value), [null, null]));
test("a stopped predecessor breaks the next line", () => assert.equal(C.series([sample(-120000, { running: false }), sample(-60000)], "requests", 1, now)[1].breakBefore, true));
test("a missing time window breaks the line", () => assert.equal(C.series([sample(-240000), sample(-60000)], "requests", 1, now)[1].breakBefore, true));
test("one valid point surrounded by two gaps always gets a marker", () => {
  const points = C.series([sample(-180000, { running: false }), sample(-120000), sample(-60000, { running: false })], "requests", 1, now);
  assert.deepEqual(C.isolatedPoints(points), [points[1]]);
});
test("the first isolated point after a restart gets a marker", () => {
  const points = C.series([sample(-180000), sample(-120000), sample(-60000, { generation: 2 })], "requests", 1, now);
  assert.deepEqual(C.isolatedPoints(points), [points[2]]);
});
test("connected zero-valued points need no isolated markers", () => {
  const points = C.series([sample(-120000, { requests: 0 }), sample(-60000, { requests: 0 })], "requests", 1, now);
  assert.deepEqual(C.isolatedPoints(points), []);
});
test("time selection omits old or future samples", () => assert.equal(C.series([sample(-3600001), sample(-1000), sample(1000)], "requests", 1, now).length, 1));
test("histogram converts cumulative buckets into disjoint counts", () => assert.deepEqual(C.histogram([{ upper_bound_micros: 1000, count: 2 }, { upper_bound_micros: 5000, count: 5 }, { upper_bound_micros: null, count: 6 }]), [{ label: "≤ 1 ms", count: 2 }, { label: "> 1–5 ms", count: 3 }, { label: "> 5 ms", count: 1 }]));
test("missing counter is not displayed as zero", () => assert.equal(C.format(undefined), "—"));

// Catch accidental dangling hardcoded element references before the browser run.
test("all static app element references exist in HTML", () => {
  const html = fs.readFileSync(path.join(root, "web/index.html"), "utf8");
  const script = fs.readFileSync(path.join(root, "web/app.js"), "utf8");
  const ids = new Set([...html.matchAll(/\bid="([^"]+)"/g)].map((match) => match[1]));
  for (const [, id] of script.matchAll(/\$\("([^"]+)"\)/g)) assert.ok(ids.has(id), `missing element #${id}`);
});

const P = globalThis.PariSession;
function deferred() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
const response = (status, data) => ({ status, ok: status >= 200 && status < 300, json: async () => data });
async function asyncTest(title, run) { await run(); passed += 1; console.log(`ok ${passed} - ${title}`); }

await asyncTest("a delayed old-session 401 cannot log out the new session", async () => {
  const pending = deferred(); let unauthorized = 0;
  const client = P.createClient({ fetcher: () => pending.promise, onUnauthorized: () => { unauthorized += 1; } });
  client.setToken("old"); const request = client.request("status");
  client.setToken(null); client.setToken("new"); pending.resolve(response(401, {}));
  await assert.rejects(request, P.isStale); assert.equal(client.token, "new"); assert.equal(unauthorized, 0);
});
await asyncTest("a delayed old status cannot publish statistics after logout", async () => {
  const pending = deferred(); const client = P.createClient({ fetcher: () => pending.promise }); let rendered = null;
  client.setToken("old"); const request = client.request("status").then((value) => { rendered = value; });
  client.setToken(null); pending.resolve(response(200, { requests: 42 }));
  await assert.rejects(request, P.isStale); assert.equal(rendered, null);
});
await asyncTest("a delayed JSON body belongs to its original session", async () => {
  const body = deferred(); const started = deferred();
  const client = P.createClient({ fetcher: async () => ({ ok: true, status: 200, json: () => { started.resolve(); return body.promise; } }) });
  client.setToken("old"); const request = client.request("stats"); await started.promise;
  client.setToken("new"); body.resolve({ samples: [{ requests: 999 }] });
  await assert.rejects(request, P.isStale); assert.equal(client.token, "new");
});
await asyncTest("old transport failures are stale, not new-session offline errors", async () => {
  const pending = deferred(); const client = P.createClient({ fetcher: () => pending.promise });
  client.setToken("old"); const request = client.request("status"); client.setToken("new"); pending.reject(new Error("offline"));
  await assert.rejects(request, P.isStale);
});
await asyncTest("even reusing a token changes the session epoch", async () => {
  const pending = deferred(); const client = P.createClient({ fetcher: () => pending.promise });
  client.setToken("same"); const owner = client.snapshot(); const request = client.request("status");
  client.setToken("same"); pending.resolve(response(200, {}));
  await assert.rejects(request, P.isStale); assert.equal(client.current(owner), false);
});
await asyncTest("a current 401 clears only the session that made the request", async () => {
  let unauthorized = 0;
  const client = P.createClient({ fetcher: async () => response(401, {}), onUnauthorized: () => { unauthorized += 1; } });
  client.setToken("current"); await assert.rejects(client.request("status"), /HTTP 401/);
  assert.equal(client.token, null); assert.equal(unauthorized, 1);
});
await asyncTest("authorization is captured at dispatch and conflicts keep the session", async () => {
  let sent;
  const client = P.createClient({ fetcher: async (_path, options) => { sent = options.headers.Authorization; return response(409, {}); } });
  client.setToken("current"); await assert.rejects(client.request("config", "PUT", {}), /配置版本已变化/);
  assert.equal(sent, "Bearer current"); assert.equal(client.token, "current");
});
console.log(`Passed ${passed} web contract tests.`);

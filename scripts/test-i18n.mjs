#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";
import { test } from "node:test";

const source = fs.readFileSync(new URL("../web/i18n.js", import.meta.url), "utf8");
function runtime(options = {}) {
  const context = vm.createContext({ ...options });
  vm.runInContext(source, context);
  return context.PariI18n;
}
test("language selection prefers saved choice, then supported browser languages", () => {
  assert.equal(runtime({ localStorage: { getItem: () => "en" }, navigator: { languages: ["zh-TW"] } }).locale, "en");
  assert.equal(runtime({ navigator: { languages: ["fr", "en-GB"] } }).locale, "en");
  assert.equal(runtime({ navigator: { languages: ["zh-TW"] } }).locale, "zh-CN");
  assert.equal(runtime({ navigator: { languages: ["fr"] } }).locale, "zh-CN");
});
test("storage restrictions keep switching usable; interpolation stays plain text", () => {
  const i = runtime({ localStorage: { getItem() { throw Error(); }, setItem() { throw Error(); } } });
  i.register("test", { greeting: ["你好，{name}", "Hello, {name}"] });
  i.setLocale("en");
  assert.equal(i.t("test.greeting", { name: "<script>$&</script>" }), "Hello, <script>$&</script>");
  assert.equal(i.t("missing.key"), "missing.key");
});
test("bindings translate in place and leave input values and focus alone", () => {
  const i = runtime(), attrs = new Map();
  const node = { value: "unfinished: draft", checked: true, textContent: "", setAttribute: (k, v) => attrs.set(k, v), getAttribute: k => attrs.get(k), removeAttribute: k => attrs.delete(k) };
  i.register("test", { label: ["用户名", "Username"], count: ["共 {count} 条", "{count} entries"] });
  i.bind(node, "test.label", {}, "aria-label");
  i.setLocale("en"); i.apply({ querySelectorAll: () => [node] });
  assert.equal(attrs.get("aria-label"), "Username");
  assert.equal(node.value, "unfinished: draft"); assert.equal(node.checked, true);
  let count = 1; i.bind(node, "test.count", () => ({ count }));
  count = 2; i.apply({ querySelectorAll: () => [node] }); assert.equal(node.textContent, "2 entries");
  i.clear(node); assert.equal(attrs.size, 1); // Only the translated aria-label remains.
});

function withCatalogs() {
  const i = runtime();
  const context = vm.createContext({ PariI18n: i, structuredClone });
  for (const name of ["console", "settings", "views"]) vm.runInContext(fs.readFileSync(new URL(`../web/locales-${name}.js`, import.meta.url), "utf8"), context);
  return { i, context };
}
test("every translation has both languages and matching interpolation fields", () => {
  const { i } = withCatalogs();
  for (const [key, [zh, en]] of Object.entries(i.messages)) {
    assert.ok(zh && en, key);
    assert.doesNotMatch(en, /[\u3400-\u9fff]/u, key);
    const fields = text => [...text.matchAll(/\{(\w+)\}/g)].map(match => match[1]).sort();
    assert.deepEqual(fields(zh), fields(en), key);
  }
});
test("static page and app references resolve; Chinese placeholders are translatable", () => {
  const { i } = withCatalogs();
  const html = fs.readFileSync(new URL("../web/index.html", import.meta.url), "utf8");
  for (const [, key] of html.matchAll(/data-i18n(?:-aria-label|-title|-placeholder)?="([^"]+)"/g)) assert.ok(i.messages[key], key);
  for (const file of ["app.js", "session.js"]) {
    const text = fs.readFileSync(new URL(`../web/${file}`, import.meta.url), "utf8");
    for (const [, key] of text.matchAll(/"((?:app|api)\.[a-zA-Z]+)"/g)) assert.ok(i.messages[key], key);
  }
  for (const [, tag] of html.matchAll(/(<[^>]+>)/g)) {
    for (const attr of ["placeholder", "aria-label", "title"]) {
      if (new RegExp(` ${attr}="[^"\\n]*[\\u3400-\\u9fff]`).test(tag)) assert.ok(tag.includes(`data-i18n-${attr}=`), tag);
    }
  }
  assert.ok(!html.includes("监控刷新不会覆盖草稿"));
});
test("settings retain schema paths while metadata changes language", () => {
  const { i, context } = withCatalogs();
  vm.runInContext(fs.readFileSync(new URL("../web/settings.js", import.meta.url), "utf8"), context);
  const { pages } = context.PariSettings;
  const references = [];
  for (const page of Object.values(pages)) {
    references.push(page.titleKey, page.introKey);
    for (const group of page.groups) {
      references.push(group.titleKey, group.helpKey);
      if (group.optional) references.push(group.enableKey);
      for (const field of group.fields) references.push(field.labelKey, ...(field.helpKey ? [field.helpKey] : []), ...(field.options || []).map(option => option[1]));
    }
  }
  for (const key of references) assert.ok(i.messages[key], key);
  const path = pages.dns.groups[0].fields[0].path;
  i.setLocale("zh-CN"); assert.equal(pages.dns.title, "DNS 设置");
  i.setLocale("en"); assert.equal(pages.dns.title, "DNS settings");
  assert.equal(pages.dns.groups[0].fields[0].path, path);
});
test("only the language preference is stored", () => {
  const writes = [];
  const i = runtime({ localStorage: { getItem: () => "bad-value", setItem: (...args) => writes.push(args) } });
  i.setLocale("en-US"); i.setLocale("zh-CN");
  assert.deepEqual(writes, [["parins.language", "en"], ["parins.language", "zh-CN"]]);
});
test("field validation uses console language instead of browser error text", () => {
  const { i, context } = withCatalogs();
  vm.runInContext(fs.readFileSync(new URL("../web/settings.js", import.meta.url), "utf8"), context);
  const settings = context.PariSettings, controls = new Map();
  for (const group of Object.values(settings.pages).flatMap(page => page.groups)) {
    if (group.optional) controls.set(`#enable-${group.optional}`, { checked: false });
    for (const field of group.fields) controls.set(`[data-path="${field.path}"]`, { value: "", checked: false, dataset: { initialValue: field.type === "checkbox" ? "false" : "" }, checkValidity: () => true });
  }
  const input = { value: "-1", dataset: { initialValue: "2000" }, checkValidity: () => false, validity: { rangeUnderflow: true }, validationMessage: "BROWSER-SPECIFIC-TEXT" };
  controls.set('[data-path="query_timeout_ms"]', input);
  let failure;
  try { settings.read({ querySelector: selector => controls.get(selector) }, { query_timeout_ms: 2000 }); } catch (error) { failure = error; }
  assert.equal(failure.key, "settings.error.invalid");
  i.setLocale("en"); assert.match(i.t(failure.key, failure.params), /1 to 60000/);
  i.setLocale("zh-CN"); assert.match(i.t(failure.key, failure.params), /1 到 60000/);
  assert.ok(!i.t(failure.key, failure.params).includes("BROWSER-SPECIFIC-TEXT"));
  assert.equal(input.value, "-1");
});

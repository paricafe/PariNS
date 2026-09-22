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

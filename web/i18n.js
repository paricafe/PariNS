"use strict";

// Presentation only: changing language never reads, validates or rewrites a draft.
globalThis.PariI18n = (() => {
  const messages = Object.create(null), parameters = new WeakMap();
  const attributes = ["textContent", "aria-label", "placeholder", "title"];
  const marker = (attribute) => attribute === "textContent" ? "data-i18n" : `data-i18n-${attribute}`;
  const supported = (value) => /^zh(?:-|$)/i.test(value || "") ? "zh-CN" : /^en(?:-|$)/i.test(value || "") ? "en" : null;
  let saved;
  try { saved = globalThis.localStorage?.getItem("parins.language"); } catch { /* Browser storage can be disabled. */ }
  let locale = supported(saved) || (globalThis.navigator?.languages || [globalThis.navigator?.language]).map(supported).find(Boolean) || "zh-CN";
  function register(namespace, entries) {
    for (const [key, pair] of Object.entries(entries)) {
      const name = `${namespace}.${key}`;
      if (messages[name] || pair.length !== 2 || pair.some((text) => typeof text !== "string")) throw new Error(`Invalid translation: ${name}`);
      messages[name] = Object.freeze([...pair]);
    }
  }
  function t(key, params = {}) {
    const values = typeof params === "function" ? params() : params;
    return (messages[key]?.[locale === "en" ? 1 : 0] ?? key).replace(/\{(\w+)\}/g, (match, name) => Object.hasOwn(values, name) ? String(values[name]) : match);
  }
  function write(node, attribute, key, params) {
    const value = t(key, params);
    if (attribute === "textContent") node.textContent = value;
    else node.setAttribute(attribute, value);
  }
  function bind(node, key, params = {}, attribute = "textContent") {
    if (!attributes.includes(attribute)) throw new Error("Unsupported translation attribute");
    const bindings = parameters.get(node) || {};
    bindings[attribute] = params; parameters.set(node, bindings);
    node.setAttribute(marker(attribute), key);
    write(node, attribute, key, params);
    return node;
  }
  function clear(node) {
    for (const attribute of attributes) node.removeAttribute(marker(attribute));
    parameters.delete(node);
  }
  function apply(root = globalThis.document) {
    if (!root) return;
    const selector = attributes.map((attribute) => `[${marker(attribute)}]`).join(",");
    const nodes = [...(root.matches?.(selector) ? [root] : []), ...root.querySelectorAll(selector)];
    for (const node of nodes) for (const attribute of attributes) {
      const key = node.getAttribute(marker(attribute));
      if (key) write(node, attribute, key, parameters.get(node)?.[attribute] || {});
    }
    if (globalThis.document) globalThis.document.documentElement.lang = locale;
  }
  function setLocale(value) {
    locale = supported(value) || "zh-CN";
    try { globalThis.localStorage?.setItem("parins.language", locale); } catch { /* Switching works without persistence. */ }
    apply();
    globalThis.window?.dispatchEvent(new Event("parins:language"));
  }
  class MessageError extends Error {
    constructor(key, params = {}) { super(t(key, params)); this.key = key; this.params = params; }
  }
  return { register, t, bind, clear, apply, setLocale, messages, MessageError,
    get locale() { return locale; },
    number: (value, options) => new Intl.NumberFormat(locale, options).format(value),
    date: (value, options) => new Intl.DateTimeFormat(locale, options).format(new Date(value)) };
})();

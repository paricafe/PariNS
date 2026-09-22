"use strict";

// A request belongs to the session that sent it, including errors and JSON reads.
globalThis.PariSession = (() => {
  class StaleRequest extends Error {
    constructor() { super("会话已变更，忽略旧请求。"); this.name = "StaleRequest"; }
  }
  function createClient({ fetcher = globalThis.fetch, onUnauthorized = () => {} } = {}) {
    let token = null, epoch = 0;
    const snapshot = () => ({ token, epoch });
    const current = (owner) => owner.epoch === epoch && owner.token === token;
    const ensureCurrent = (owner) => { if (!current(owner)) throw new StaleRequest(); };
    const setToken = (next) => { token = next; epoch += 1; };
    async function request(path, method = "GET", body, extraHeaders = {}) {
      const owner = snapshot();
      const headers = { ...extraHeaders };
      if (owner.token) headers.Authorization = `Bearer ${owner.token}`;
      if (body !== undefined) headers["Content-Type"] = "application/json";
      let response, data;
      try {
        response = await fetcher(`/api/${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), credentials: "omit", cache: "no-store", redirect: "error" });
        ensureCurrent(owner);
        data = await response.json();
      } catch (error) { ensureCurrent(owner); throw error; }
      ensureCurrent(owner);
      if (!response.ok) {
        if (response.status === 401 && owner.token) { setToken(null); onUnauthorized(); }
        throw new Error(response.status === 409 ? "配置版本已变化。请先导出未保存内容，再重新加载最新配置后合并修改。" : data.error?.message || `请求失败（HTTP ${response.status}）`);
      }
      return data;
    }
    return { get token() { return token; }, snapshot, current, ensureCurrent, setToken, request };
  }
  return { createClient, isStale: (error) => error instanceof StaleRequest };
})();

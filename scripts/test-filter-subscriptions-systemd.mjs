// Real installed service only. Auth stays in memory; stdout contains evidence,
// never response bodies, credentials, cookies or source content.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const [stage, fixture] = process.argv.slice(2);
assert(process.platform === 'linux' && process.env.GITHUB_ACTIONS === 'true'
  && process.env.RUNNER_ENVIRONMENT === 'github-hosted' && process.env.RUNNER_OS === 'Linux',
  'requires a disposable GitHub Linux runner');
assert(['activate', 'restart', 'offline', 'frozen', 'unfrozen'].includes(stage));
assert(path.isAbsolute(fixture));
const base = 'http://127.0.0.1:3000';
const source = { id: 'fs-ci', format: 'domain_list',
  url: 'https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/d1e0e168589302c62373256855b0d8542f058bdf/natsuki-list.list' };
const directory = '/var/lib/parins-managed/filter-subscriptions';
const evidencePath = path.join(fixture, 'filter-evidence.json');
let auth;
function safeCode(value, fallback = 'unknown') {
  return typeof value === 'string' && /^[A-Za-z0-9_]{1,64}$/.test(value) ? value : fallback;
}
function httpDiagnostic(method, apiPath, actualStatus, expectedStatus, errorCode) {
  const paths = ['/api/login', '/api/config', '/api/config/validate', '/api/status', '/api/updates',
    '/api/filter/check', '/api/filter/subscriptions',
    '/api/filter/subscriptions/prepare', '/api/filter/subscriptions/refresh'];
  if (!['GET', 'POST', 'PUT'].includes(method) || !paths.includes(apiPath)
    || !Number.isInteger(actualStatus) || actualStatus < 100 || actualStatus > 599
    || !Number.isInteger(expectedStatus) || expectedStatus < 100 || expectedStatus > 599) return undefined;
  return { method, api_path: apiPath, actual_status: actualStatus,
    expected_status: expectedStatus, api_error_code: safeCode(errorCode) };
}
function request(method, endpoint, body, expected = 200, timeoutMs = 30000) {
  return new Promise((resolve, reject) => {
    const headers = { Origin: base, 'Content-Type': 'application/json' };
    if (auth) Object.assign(headers, { Cookie: auth.cookie, 'X-PariNS-Session': auth.binding });
    const req = http.request(base + endpoint, { method, headers, timeout: timeoutMs }, res => {
      let bytes = '';
      res.on('data', chunk => {
        bytes += chunk;
        if (bytes.length > 1024 * 1024) res.destroy(new Error('bounded API response exceeded'));
      });
      res.on('error', reject);
      res.on('end', () => {
        try {
          const value = JSON.parse(bytes);
          if (res.statusCode !== expected) {
            const error = new Error('unexpected API HTTP status');
            error.code = 'unexpected_http_status';
            error.http = httpDiagnostic(method, endpoint, res.statusCode, expected, value.error?.code);
            throw error;
          }
          if (value.session?.binding) {
            assert(res.headers['set-cookie']?.length === 1, 'one session cookie required');
            auth = { cookie: res.headers['set-cookie'][0].split(';')[0], binding: value.session.binding };
          }
          resolve(value);
        } catch (error) { reject(error); }
      });
    });
    req.on('timeout', () => req.destroy(new Error(`${endpoint}: timeout, mutation not replayed`)));
    req.on('error', reject);
    req.end(body === undefined ? undefined : JSON.stringify(body));
  });
}
function installationReconciled(value) {
  assert.equal(typeof value.frozen, 'boolean', 'updates.frozen must be a boolean');
  assert(value.active_operation === null || (typeof value.active_operation === 'object'
    && !Array.isArray(value.active_operation)), 'updates.active_operation must be present');
  return value.frozen === false && value.active_operation === null;
}
async function waitForInstallationReconciliation() {
  const started = Date.now();
  const deadline = started + 30000;
  let reads = 0;
  const expired = () => Object.assign(new Error('installer reconciliation deadline exceeded'),
    { code: 'installer_reconcile_timeout' });
  while (Date.now() < deadline) {
    let value;
    try {
      value = await request('GET', '/api/updates', undefined, 200, Math.max(1, deadline - Date.now()));
    } catch (error) {
      if (Date.now() >= deadline) throw expired();
      throw error;
    }
    reads++;
    if (Date.now() >= deadline) break;
    if (installationReconciled(value)) {
      const result = { reads, elapsed_ms: Date.now() - started };
      console.log(JSON.stringify({ stage: 'activate', installation_reconciled: true, ...result }));
      return result;
    }
    await delay(Math.min(1000, Math.max(0, deadline - Date.now())));
  }
  throw expired();
}
const snapshot = () => request('GET', '/api/filter/subscriptions');
function privilegedRead(file, maxBuffer = 256 * 1024) {
  return execFileSync('sudo', ['-n', 'cat', file], { maxBuffer, stdio: ['ignore', 'pipe', 'pipe'] });
}
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
function selected(evidence, rootsSynchronized = true) {
  const catalog = JSON.parse(privilegedRead(`${directory}/catalog.json`));
  const record = catalog.records.find(item => item.fingerprint === evidence.fingerprint);
  assert(record, 'catalog retains the selected source content');
  if (rootsSynchronized) assert(catalog.current.includes(record.fingerprint), 'Config-selected source remains rooted');
  assert.equal(record.sha256, evidence.sha256);
  return catalog;
}
async function operation(endpoint, body, outcome) {
  const { operation_id: id } = await request('POST', endpoint, body, 202);
  assert.match(id, /^[a-f0-9]{16}$/);
  const deadline = Date.now() + 310000;
  while (Date.now() < deadline) {
    const state = await snapshot();
    const op = [state.operation, state.recent_operation].find(item => item?.id === id);
    assert(op, 'accepted operation remains observable');
    if (op.status !== 'running') {
      assert.equal(op.status, outcome, `operation result (${op.error?.code ?? 'no error code'})`);
      return op;
    }
    await delay(250);
  }
  throw new Error('accepted operation did not finish within batch deadline');
}
async function blocked(evidence) {
  const explanation = await request('POST', '/api/filter/check', { name: evidence.domain });
  assert.equal(explanation.decision, 'blocked');
  assert.equal(explanation.witness?.source_id, source.id, 'subscription, not local rule, is the witness');
  const status = await request('GET', '/api/status');
  assert.equal(status.running, true);
  assert.equal(status.last_error, null);
  const match = /^127\.0\.0\.1:(\d+)$/.exec(status.listen);
  assert(match && Number(match[1]) > 1024);
  const header = Buffer.from('504e01000001000000000000', 'hex');
  const name = Buffer.concat(evidence.domain.split('.').map(label => Buffer.concat([Buffer.from([label.length]), Buffer.from(label)])));
  const query = Buffer.concat([header, name, Buffer.from('0000010001', 'hex')]);
  const socket = dgram.createSocket('udp4');
  try {
    const reply = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('DNS timeout')), 3000);
      socket.once('error', error => { clearTimeout(timer); reject(error); });
      socket.once('message', (bytes, peer) => {
        clearTimeout(timer);
        try { assert.equal(peer.address, '127.0.0.1'); assert.equal(peer.port, Number(match[1])); resolve(bytes); }
        catch (error) { reject(error); }
      });
      socket.send(query, Number(match[1]), '127.0.0.1');
    });
    assert(reply.length >= 12);
    assert.equal(reply.readUInt16BE(0), 0x504e);
    assert(reply[2] & 0x80);
    assert.equal(reply[3] & 15, 0);
    assert.equal(reply.readUInt16BE(6), 0);
  } finally { socket.close(); }
}

async function run() {
await request('POST', '/api/login', JSON.parse(await readFile(path.join(fixture, 'credentials.json'), 'utf8')));
if (stage === 'activate') {
  // Installer readiness and app/root reconciliation are distinct. Wait only via
  // reads before the first mutation; never replay prepare or wait in frozen tests.
  const installation_reconciliation = await waitForInstallationReconciliation();
  const config = await request('GET', '/api/config');
  assert(!config.toml.includes('[filter_subscriptions]') && !config.toml.includes('[updates]'), 'fresh fixture configuration');
  const prepared = await operation('/api/filter/subscriptions/prepare', { config_revision: config.revision, source }, 'succeeded');
  assert.match(prepared.sha256, /^[a-f0-9]{64}$/);
  assert(prepared.rules > 0);
  const bytes = privilegedRead(`${directory}/objects/${prepared.sha256}.txt`, 16 * 1024 * 1024);
  assert.equal(digest(bytes), prepared.sha256);
  const domain = bytes.toString('ascii').split(/\r?\n/).map(line => line.trim().replace(/^\./, ''))
    .find(line => line !== 'ci.invalid' && /^(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?\.)+[a-zA-Z]{2,63}$/.test(line));
  assert(domain, 'pinned list contains an ASCII test domain');
  const toml = `${config.toml}\n[updates]\nauto_check = false\n[filter_subscriptions]\nenabled = true\nmax_rules = 1000000\nmax_memory_bytes = 268435456\nmax_disk_bytes = 268435456\n[[filter_subscriptions.sources]]\nid = "${source.id}"\nname = "Pinned CI source"\nurl = "${source.url}"\nformat = "domain_list"\nenabled = true\nauto_update = false\nupdate_interval_hours = 24\n`;
  await request('POST', '/api/config/validate', { toml });
  await request('PUT', '/api/config', { revision: config.revision, toml });
  const saved = await request('GET', '/api/config');
  assert(saved.toml === toml, 'Config save retains the enabled source');
  assert.equal(saved.revision, config.revision + 1);
  const state = await snapshot();
  assert.equal(state.config_revision, saved.revision);
  const active = state.sources.find(item => item.id === source.id);
  assert(active?.active && active.ready && active.input_rules > 0);
  assert.equal(state.unavailable_reason, null);
  const evidence = { corpus_source_commit: 'd1e0e168589302c62373256855b0d8542f058bdf',
    sha256: prepared.sha256, fingerprint: active.fingerprint, domain,
    config_revision: state.config_revision, content_revision: state.content_revision,
    input_rules: state.input_rules, index_rules: state.index_rules };
  // Config owns activation. Its successful commit has no second catalog commit:
  // GC roots are synchronized at the next worker admission or non-frozen open.
  // Keep the root assertion for the following actual restart/refresh stages.
  selected(evidence, false);
  await blocked(evidence);
  await writeFile(evidencePath, JSON.stringify(evidence) + '\n', { mode: 0o600, flag: 'wx' });
  return { stage, result: 'passed', installation_reconciliation, ...evidence };
} else {
  const evidence = JSON.parse(await readFile(evidencePath, 'utf8'));
  const before = await snapshot();
  assert.equal(before.config_revision, evidence.config_revision);
  assert.equal(before.content_revision, evidence.content_revision);
  assert.equal(before.input_rules, evidence.input_rules);
  const active = before.sources.find(item => item.id === source.id);
  assert(active?.active && active.ready);
  assert.equal(before.unavailable_reason, null);
  selected(evidence);
  await blocked(evidence);
  if (stage === 'frozen') {
    assert.equal(before.operation, null);
    const catalog = privilegedRead(`${directory}/catalog.json`);
    for (const [endpoint, body] of [
      ['/api/filter/subscriptions/prepare', { config_revision: evidence.config_revision, source: { ...source, id: 'fs-unconfigured' } }],
      ['/api/filter/subscriptions/refresh', { config_revision: evidence.config_revision, source_id: source.id }],
    ]) {
      const rejected = await request('POST', endpoint, body, 409);
      assert.equal(rejected.error?.code, 'update_in_progress');
    }
    assert.equal(digest(privilegedRead(`${directory}/catalog.json`)), digest(catalog));
    assert.equal((await snapshot()).operation, null);
  } else if (stage === 'offline' || stage === 'unfrozen') {
    // Respect the real per-source 60s manual refresh budget. No mutation retries
    // or clock/catalog rewrites are used to bypass it; auto_update is disabled.
    const due = (active.last_attempt ?? 0) * 1000 + 61000;
    assert(due <= Date.now() + 62000, 'fixture source clock is not in the future');
    while (Date.now() < due) await delay(Math.min(1000, due - Date.now()));
    const result = await operation('/api/filter/subscriptions/refresh',
      { config_revision: evidence.config_revision, source_id: source.id }, stage === 'offline' ? 'failed' : 'succeeded');
    if (stage === 'offline') assert.match(result.error?.code ?? '', /^subscription_(download|timeout)$/);
    const after = await snapshot();
    assert.equal(after.content_revision, before.content_revision);
    assert.equal(after.generation, before.generation);
    selected(evidence);
    await blocked(evidence);
  }
  return { stage, result: 'passed', sha256: evidence.sha256,
    content_revision: evidence.content_revision, input_rules: evidence.input_rules };
}
}

const artifacts = fileURLToPath(new URL('../artifacts/filter-subscriptions/', import.meta.url));
await mkdir(artifacts, { recursive: true, mode: 0o700 });
const artifact = path.join(artifacts, `${stage}.json`);
const installedBuild = JSON.parse(privilegedRead('/var/lib/parins-updater/private/journal.json')).installed.build;
const initial = { stage, result: 'running', app_source_commit: installedBuild.source_commit,
  corpus_source_commit: 'd1e0e168589302c62373256855b0d8542f058bdf',
  started_at: new Date().toISOString() };
await writeFile(artifact, JSON.stringify(initial) + '\n', { mode: 0o600 });
try {
  const outcome = await run();
  const result = { ...initial, ...outcome, corpus_sha256: outcome.sha256, finished_at: new Date().toISOString() };
  await writeFile(artifact, JSON.stringify(result, null, 2) + '\n', { mode: 0o600 });
  console.log(JSON.stringify(result));
} catch (error) {
  // Error messages can contain assertion values; only store a stable identifier.
  const code = safeCode(error.code, 'fixture_failed');
  const location = String(error.stack).match(/test-filter-subscriptions-systemd\.mjs:\d+:\d+/)?.[0] ?? 'unknown';
  const http = httpDiagnostic(error.http?.method, error.http?.api_path,
    error.http?.actual_status, error.http?.expected_status, error.http?.api_error_code);
  await writeFile(artifact, JSON.stringify({ ...initial, result: 'failed', code,
    location, ...http, finished_at: new Date().toISOString() }, null, 2) + '\n', { mode: 0o600 });
  console.error(JSON.stringify({ stage, code, location, ...http }));
  process.exitCode = 1;
}

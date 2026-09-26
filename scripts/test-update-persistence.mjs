// Dedicated two-version fixture. No system service, public bind, external DNS,
// real credentials, business-data restore, or production update is involved.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import { once } from 'node:events';
import { appendFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

const [root, baseline] = process.argv.slice(2);
assert(root && baseline);
const old = path.join(root, 'old-parins');
const current = path.join(root, 'current-parins');
const state = path.join(root, 'state');
await mkdir(state, { mode: 0o700 });
const password = 'isolated-persistence-fixture-only';
const config = `listen = "127.0.0.1:0"
query_timeout_ms = 200
tcp_io_timeout_ms = 500
shutdown_grace_ms = 200
max_inflight = 16
max_tcp_connections = 8
[upstreams]
servers = ["127.0.0.1:9"]
[filter]
enabled = true
block_exact = ["persistence.test"]
[query_log]
enabled = true
max_entries = 100
retention_secs = 3600
[storage]
flush_interval_ms = 100
`;
const evidence = { baseline_commit: baseline, platform: process.platform, architecture: process.arch, stages: [] };
let previousLogIds = [];
for (const [name, executable] of [['old', old], ['current', current]]) {
  evidence[`${name}_sha256`] = createHash('sha256').update(await readFile(executable)).digest('hex');
}
const build = spawnSync(current, ['--build-info=json'], { encoding: 'utf8' });
assert.equal(build.status, 0);
evidence.current_build = JSON.parse(build.stdout);
assert.notEqual(evidence.old_sha256, evidence.current_sha256);

async function port() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const value = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return value;
}

async function launch(executable, label) {
  const address = `127.0.0.1:${await port()}`;
  const child = spawn(executable, ['--manage', '--state-dir', state, '--web-listen', address], {
    env: { PATH: process.env.PATH, LANG: 'C' }, stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', bytes => { stderr = (stderr + bytes).slice(-16384); });
  const exited = new Promise(resolve => child.once('exit', (code, signal) => resolve({ code, signal })));
  // The new scheduler's first check is >=60s. These independent hard deadlines
  // stop the process before it can perform a check even if assertions hang.
  const graceful = setTimeout(() => child.kill('SIGTERM'), 35000);
  const hard = setTimeout(() => child.kill('SIGKILL'), 40000);
  child.once('exit', () => { clearTimeout(graceful); clearTimeout(hard); });
  let auth;
  const request = (method, endpoint, body, extra = {}) => new Promise((resolve, reject) => {
    const data = body === undefined ? '' : JSON.stringify(body);
    const headers = { Origin: `http://${address}`, 'Content-Type': 'application/json', ...extra };
    if (auth) Object.assign(headers, { Cookie: auth.cookie, 'X-PariNS-Session': auth.binding });
    const req = http.request(`http://${address}${endpoint}`, { method, headers, timeout: 3000 }, res => {
      let bytes = '';
      res.on('data', chunk => { bytes += chunk; if (bytes.length > 1024 * 1024) res.destroy(new Error('bounded fixture response')); });
      res.on('error', reject);
      res.on('end', () => {
        try {
          const parsed = JSON.parse(bytes);
          assert.equal(res.statusCode, 200, `${label} ${endpoint}: ${bytes}`);
          if (parsed.session?.binding) auth = { cookie: res.headers['set-cookie'][0].split(';')[0], binding: parsed.session.binding };
          resolve(parsed);
        } catch (error) { reject(error); }
      });
    });
    req.on('timeout', () => req.destroy(new Error('fixture request timeout')));
    req.on('error', reject); req.end(data);
  });
  const stop = async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    const result = await exited;
    clearTimeout(graceful); clearTimeout(hard);
    await appendFile(path.join(root, 'process-results.jsonl'), JSON.stringify({ label, ...result }) + '\n');
    assert.equal(result.code, 0, `${label}: did not exit cleanly (${result.signal})`);
  };
  return { request, stop, exited, get stderr() { return stderr; }, async ready() {
    for (let attempt = 0; attempt < 80; attempt++) {
      if (child.exitCode !== null) throw new Error(`${label} exited before ready: ${stderr}`);
      try { await request('GET', '/api/session'); return; } catch { await delay(50); }
    }
    throw new Error(`${label} did not become ready`);
  }, async login() { await request('POST', '/api/login', { username: 'admin', password }); } };
}

async function dns(address) {
  const [host, value] = address.split(':');
  assert.equal(host, '127.0.0.1');
  const socket = dgram.createSocket('udp4');
  // One A query for a local blocked name; the upstream is never contacted.
  const query = Buffer.from('1234010000010000000000000b70657273697374656e636504746573740000010001', 'hex');
  try {
    const received = once(socket, 'message');
    socket.send(query, Number(value), host);
    const [reply] = await Promise.race([received, delay(2000).then(() => { throw new Error('DNS fixture timeout'); })]);
    assert.equal(reply.readUInt16BE(0), 0x1234);
    assert.equal(reply[3] & 15, 0); // default filter produces NOERROR/no answers
    assert.equal(reply.readUInt16BE(6), 0);
  } finally { socket.close(); }
}

async function observe(server, expected) {
  for (let attempt = 0; attempt < 50; attempt++) {
    const stats = await server.request('GET', '/api/stats');
    const logs = (await server.request('POST', '/api/query-log/list', {})).page;
    if (stats.totals.metrics.counters.requests === expected && logs.total === expected) {
      assert.equal(stats.storage.health, 'healthy');
      assert(stats.samples.length > 0, 'persisted trend samples remain readable');
      assert(logs.entries.every(entry => entry.name === 'persistence.test.' && entry.status === 'blocked'));
      const logIds = logs.entries.map(entry => entry.id);
      assert(previousLogIds.every(id => logIds.includes(id)), 'previous persisted log identities remain present');
      previousLogIds = logIds;
      return { requests: stats.totals.metrics.counters.requests, log_entries: logs.total,
        totals_epoch: stats.totals.epoch, history_epoch: stats.history_epoch,
        storage: stats.storage.health, samples: stats.samples.length };
    }
    await delay(100);
  }
  throw new Error(`persisted requests/logs did not reach ${expected}`);
}

let active;
try {
  active = await launch(old, 'old-write'); await active.ready();
  const token = (await readFile(path.join(state, 'setup-token'), 'utf8')).trim();
  await active.request('POST', '/api/setup', { username: 'admin', password, toml: config }, { 'X-PariNS-Setup': token });
  let status = await active.request('GET', '/api/status');
  assert.equal(status.running, true);
  await dns(status.listen); await dns(status.listen);
  evidence.stages.push({ stage: 'old-write', ...await observe(active, 2) });
  await active.stop(); active = undefined;
  const originalState = await readFile(path.join(state, 'state.json'));
  const database = path.join(state, 'runtime', 'observability.sqlite3');
  assert((await readFile(database)).subarray(0, 16).equals(Buffer.from('SQLite format 3\0')));

  active = await launch(current, 'current-read-write'); await active.ready(); await active.login();
  evidence.stages.push({ stage: 'current-read-old', ...await observe(active, 2) });
  status = await active.request('GET', '/api/status'); assert.equal(status.running, true);
  await dns(status.listen); await dns(status.listen);
  evidence.stages.push({ stage: 'current-write', ...await observe(active, 4) });
  await active.stop(); active = undefined;
  assert.deepEqual(await readFile(path.join(state, 'state.json')), originalState, 'normal startup must not migrate saved config');

  active = await launch(old, 'old-reread-write'); await active.ready(); await active.login();
  evidence.stages.push({ stage: 'old-read-current', ...await observe(active, 4) });
  status = await active.request('GET', '/api/status'); assert.equal(status.running, true);
  await dns(status.listen); await dns(status.listen);
  evidence.stages.push({ stage: 'old-write-again', ...await observe(active, 6) });
  await active.stop(); active = undefined;
  assert.deepEqual(await readFile(path.join(state, 'state.json')), originalState);

  active = await launch(current, 'current-save-updates'); await active.ready(); await active.login();
  const saved = await active.request('GET', '/api/config');
  await active.request('PUT', '/api/config', { revision: saved.revision, toml: `${saved.toml}\n[updates]\nauto_check = false\n` });
  evidence.stages.push({ stage: 'current-after-old-rewrite', ...await observe(active, 6) });
  await active.stop(); active = undefined;

  // No data restoration here: exercise the actual old parser against the
  // candidate-written state and preserve its failure as the manual boundary.
  active = await launch(old, 'old-rejects-new-config');
  const rejection = await active.exited;
  assert.notEqual(rejection.code, 0);
  assert.match(active.stderr, /unknown field [`']updates[`']/);
  evidence.stages.push({ stage: 'old-after-new-config', result: 'expected rejection: unknown field updates' });
  // stop() requires success, so this expected failed process is already dead.
  active = undefined;
  evidence.conclusion = 'Known v0.1.4 data round-trip passes without config mutation; saving updates is not backward-readable. v0.1.4 has no durable epoch. This is not a same-epoch release gate pass.';
  await writeFile(path.join(root, 'evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} finally {
  if (active) await active.stop();
}

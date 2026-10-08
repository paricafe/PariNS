// Disposable installed-service capacity fixture. Never log configuration or auth.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import { lookup } from 'node:dns/promises';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import http from 'node:http';
import https from 'node:https';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { gunzipSync, gzipSync } from 'node:zlib';
import { cpuList, linuxUsage, nameLabels, parse, query, verifyLinuxServer } from './lib/wire-bench.mjs';
import { aResponse, distribution, dueCount, ioStat, oracle, plannedCount, probe, runWirePhase } from './lib/managed-wire.mjs';

const MiB = 1024 * 1024;
const SLO = Object.freeze({ rate: 1000, steady_ms: 60000, update_ms: 120000,
  refresh_at_ms: 15000, client_deadline_ms: 1000, server_deadline_ms: 20000,
  steady_p99_ms: 20, update_p99_ms: 50, timeout_fraction: 0.001,
  correct_qps: 999, steady_rss_bytes: 256 * MiB, update_rss_bytes: 512 * MiB,
  startup_ms: 30000, startup_rss_bytes: 512 * MiB });
const SOURCES = [
  { id: 'lifecycle-corpus', format: 'domain_list', url: 'https://raw.githubusercontent.com/Natsuki-Kaede/Natsuki-List/d1e0e168589302c62373256855b0d8542f058bdf/natsuki-list.list' },
  { id: 'lifecycle-mutable', format: 'domain_list', url: 'https://raw.githubusercontent.com/paricafe/PariNS/acceptance/filter-https-source/filter-lifecycle.list' },
];
const CORPUS_SHA = 'd68e37b2a861e6e8ef85568db4237bb3e18d1a9f2476323b3dba977fe850af09';
const CONTENT = { A: 'lifecycle-a.test\ncommon.lifecycle.test\n', B: 'lifecycle-b.test\ncommon.lifecycle.test\n' };
const REPRESENTATION = 'text/plain;accept-encoding=gzip, identity;v=1';
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const base = 'http://127.0.0.1:3000';
const UNIT = 'parins-managed.service';
const FILTER_DIR = '/var/lib/parins-managed/filter-subscriptions';
const safeCode = value => typeof value === 'string' && /^[a-zA-Z0-9_]{1,64}$/.test(value) ? value : 'fixture_failed';
const fail = code => Object.assign(new Error(code), { code });
function itemFor(sequence, phase, active) {
  switch (sequence % 4) {
    case 0: return { group: 'fresh_cache', name: 'fresh.lifecycle.test', blocked: false };
    case 1: return { group: 'unique_miss', name: `m${phase}-${sequence}.lifecycle.test`, blocked: false };
    case 2: return { group: 'subscription_common', name: 'common.lifecycle.test', blocked: active };
    default: return { group: 'allow_local', name: 'allow.local.lifecycle.test', blocked: false };
  }
}
function cnameResponse(packet, target) {
  const end = parse(packet).questionEnd;
  const header = Buffer.from(packet.subarray(0, end));
  header.writeUInt16BE(0x8180, 2); header.writeUInt16BE(2, 6);
  header.writeUInt16BE(0, 8); header.writeUInt16BE(0, 10);
  const encoded = query(0, target).subarray(12, -4);
  const cname = Buffer.from('c00c0005000100000e100000', 'hex');
  cname.writeUInt16BE(encoded.length, 10);
  const a = Buffer.from('0001000100000e100004c000022a', 'hex');
  return Buffer.concat([header, cname, encoded, encoded, a]);
}

let auth;
function httpDiagnostic(method, apiPath, status, expected, code) {
  if (!['GET', 'POST', 'PUT'].includes(method)
    || !['/api/session', '/api/login', '/api/updates', '/api/config', '/api/config/validate', '/api/status',
      '/api/filter/check', '/api/filter/subscriptions', '/api/filter/subscriptions/prepare', '/api/filter/subscriptions/refresh'].includes(apiPath)
    || !Number.isInteger(status) || status < 100 || status > 599
    || !Number.isInteger(expected) || expected < 100 || expected > 599) return undefined;
  return { method, api_path: apiPath, actual_status: status, expected_status: expected, api_error_code: safeCode(code) };
}
function request(method, endpoint, body, expected = 200, timeout = 30000) {
  return new Promise((resolve, reject) => {
    const headers = { Origin: base, 'Content-Type': 'application/json' };
    if (auth) Object.assign(headers, { Cookie: auth.cookie, 'X-PariNS-Session': auth.binding });
    const req = http.request(base + endpoint, { method, headers, timeout }, res => {
      let bytes = '';
      res.on('data', chunk => { bytes += chunk; if (bytes.length > MiB) res.destroy(fail('api_body_limit')); });
      res.on('error', reject);
      res.on('end', () => {
        try {
          const value = JSON.parse(bytes);
          if (res.statusCode !== expected) {
            const error = fail(`http_${res.statusCode}_${safeCode(value.error?.code)}`);
            error.http = httpDiagnostic(method, endpoint, res.statusCode, expected, value.error?.code);
            throw error;
          }
          if (value.session?.binding) {
            assert.equal(res.headers['set-cookie']?.length, 1);
            auth = { cookie: res.headers['set-cookie'][0].split(';')[0], binding: value.session.binding };
          }
          resolve(value);
        } catch (error) { reject(error); }
      });
    });
    req.on('timeout', () => req.destroy(fail(method === 'GET' ? 'api_read_timeout' : 'api_timeout_mutation_not_replayed')));
    req.on('error', reject); req.end(body === undefined ? undefined : JSON.stringify(body));
  });
}
const snapshot = () => request('GET', '/api/filter/subscriptions');
async function waitForManagement() {
  const deadline = performance.now() + 30000;
  while (performance.now() < deadline) {
    try {
      const session = await request('GET', '/api/session', undefined, 200, 1000);
      assert.equal(session.setup_required, false, 'installer completed setup');
      assert.equal(session.authenticated, false, 'initial GET has no session');
      assert.equal(session.session, null);
      return;
    } catch (error) {
      if (!['ECONNREFUSED', 'ECONNRESET', 'api_read_timeout'].includes(error.code)) throw error;
    }
    await delay(250);
  }
  throw fail('management_readiness_timeout');
}
async function waitForInstallation() {
  const deadline = performance.now() + 30000;
  while (performance.now() < deadline) {
    const installation = await request('GET', '/api/updates');
    assert.equal(typeof installation.frozen, 'boolean');
    assert(installation.active_operation === null || typeof installation.active_operation === 'object');
    if (!installation.frozen && installation.active_operation === null) return;
    await delay(500);
  }
  throw fail('installation_reconcile_timeout');
}
function selected(state) {
  return { generation: state.generation, content_revision: state.content_revision,
    input_rules: state.input_rules, index_rules: state.index_rules, index_bytes: state.index_bytes,
    retained_bytes: state.retained_bytes, disk_bytes: state.disk_bytes,
    sources: state.sources.map(s => ({ id: s.id, fingerprint: s.fingerprint,
      active: s.active, ready: s.ready, input_rules: s.input_rules,
      last_attempt: s.last_attempt, last_success: s.last_success })) };
}
async function operation(endpoint, body, deadline = 310000, observe) {
  const started = performance.now();
  const accepted = await request('POST', endpoint, body, 202);
  assert.match(accepted.operation_id, /^[a-f0-9]{16}$/);
  observe?.({ id: accepted.operation_id, status: 'running' });
  while (performance.now() - started < deadline) {
    const state = await snapshot();
    const op = [state.operation, state.recent_operation].find(value => value?.id === accepted.operation_id);
    assert(op, 'accepted operation remains observable');
    if (op.status !== 'running') {
      observe?.(op, state);
      if (op.status !== 'succeeded') throw fail(`operation_${safeCode(op.error?.code)}`);
      return { op, state, observed_ms: performance.now() - started };
    }
    await delay(100);
  }
  throw fail('operation_deadline');
}
function sourceHeaders(binding) {
  const headers = { 'User-Agent': 'PariNS-subscriptions/1', Accept: 'text/plain',
    'Accept-Encoding': 'gzip, identity', Connection: 'close' };
  if (binding?.etag) headers['If-None-Match'] = binding.etag;
  else if (binding?.last_modified) headers['If-Modified-Since'] = binding.last_modified;
  return headers;
}
function sourceConditional(binding) {
  const kind = binding?.etag ? 'etag' : binding?.last_modified ? 'last_modified' : null;
  return { kind, validator_sha256: kind ? digest(binding[kind]) : null,
    content_sha256: binding?.content_sha256 ?? null };
}
function sourceBodySha(status, encoding, bytes) {
  if (status !== 200) return null; // In particular, 304 never proves the next fixture is ready.
  assert(bytes.length <= 4096, 'bounded fixture transfer');
  const normalized = (encoding ?? 'identity').toLowerCase();
  if (!['gzip', 'identity'].includes(normalized)) throw fail('source_fixture_encoding');
  const decoded = normalized === 'gzip' ? gunzipSync(bytes, { maxOutputLength: 4096 }) : bytes;
  if (decoded.length > 4096) throw fail('source_fixture_body_limit');
  return digest(decoded);
}
function sourceBinding(pid, state, current) {
  const source = state.sources.find(s => s.id === SOURCES[1].id); assert(source);
  const catalog = JSON.parse(privileged(pid, 'cat', `${FILTER_DIR}/catalog.json`));
  assert.equal(catalog.content_revision, state.content_revision);
  const records = catalog.records.filter(r => r.fingerprint === source.fingerprint);
  assert.equal(records.length, 1); const record = records[0];
  assert.equal(record.sha256, digest(CONTENT[current]));
  assert.equal(fileIdentity(pid, `${FILTER_DIR}/objects/${record.sha256}.txt`).sha256, record.sha256);
  const binding = record.validators;
  assert(binding, 'prepared source has committed validators');
  assert.equal(binding.final_url, SOURCES[1].url);
  assert.equal(binding.representation, REPRESENTATION);
  assert.equal(binding.content_sha256, record.sha256);
  for (const key of ['etag', 'last_modified']) if (binding[key] !== null && binding[key] !== undefined) {
    assert.equal(typeof binding[key], 'string'); assert(binding[key].length > 0 && binding[key].length <= 1024);
    http.validateHeaderValue(key, binding[key]);
  }
  return binding;
}
async function sourceProbe(binding, evidence) {
  const url = new URL(SOURCES[1].url), controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 10000);
  const signal = controller.signal;
  try {
    // OS getaddrinfo order, as used by the Rust reader; do not race address families.
    const addresses = await Promise.race([lookup(url.hostname, { all: true, verbatim: true }),
      new Promise((_, reject) => signal.addEventListener('abort', () => reject(fail('source_probe_timeout')), { once: true }))]);
    assert(addresses.length > 0 && addresses.length <= 16);
    for (const address of addresses) {
      let connected = false;
      const peer = { address: address.address, family: address.family }; evidence.connection_attempts.push(peer);
      try {
        return await new Promise((resolve, reject) => {
          const req = https.request(url, { method: 'GET', agent: false, signal,
            servername: url.hostname, ALPNProtocols: ['http/1.1'], autoSelectFamily: false,
            lookup: (_host, _options, callback) => callback(null, address.address, address.family),
            headers: sourceHeaders(binding) }, res => {
            evidence.probe_http_status = res.statusCode;
            evidence.peer = { address: res.socket.remoteAddress, family: res.socket.remoteFamily };
            const encoding = (res.headers['content-encoding'] ?? 'identity').toLowerCase();
            evidence.probe_content_encoding = ['gzip', 'identity'].includes(encoding) ? encoding : 'unsupported';
            let bytes = Buffer.alloc(0);
            res.on('data', chunk => {
              bytes = Buffer.concat([bytes, chunk]);
              if (bytes.length > 4096) res.destroy(fail('source_fixture_body_limit'));
            });
            res.on('error', reject);
            res.on('end', () => {
              try { resolve(sourceBodySha(res.statusCode, res.headers['content-encoding'], bytes)); }
              catch (error) { reject(error); }
            });
          });
          req.on('socket', socket => socket.once('connect', () => { connected = true; peer.tcp_connected = true; }));
          req.on('error', reject); req.end();
        });
      } catch (error) {
        peer.error = safeCode(error.code);
        // Rust selects the first TCP-reachable address; TLS/HTTP failure does not select another peer.
        if (connected || signal.aborted) throw error;
      }
    }
    throw fail('source_probe_connect_failed');
  } finally { clearTimeout(timeout); }
}
async function sourceReady(round, expected, binding, evidence) {
  console.log(`LIFECYCLE_SOURCE_WAIT round=${round} expected=${expected}`);
  const started = performance.now(); let reads = 0;
  const result = { round, expected, expected_sha256: digest(CONTENT[expected]), representation: REPRESENTATION,
    conditional: sourceConditional(binding), probes: [] };
  evidence.push(result);
  while (performance.now() - started < 600000) {
    const attempt = { read: ++reads, connection_attempts: [] }; result.probes.push(attempt);
    try { attempt.decoded_sha256 = await sourceProbe(binding, attempt); }
    catch (error) { attempt.error = safeCode(error.code); throw error; }
    result.reads = reads; result.readiness_wait_ms = performance.now() - started;
    if (attempt.probe_http_status === 200 && attempt.decoded_sha256 === result.expected_sha256) {
      result.sha256 = attempt.decoded_sha256;
      console.log(JSON.stringify({ event: 'source_ready', round, expected, reads,
        sha256: result.sha256, readiness_wait_ms: result.readiness_wait_ms }));
      return result;
    }
    await delay(5000);
  }
  throw fail('source_fixture_readiness_timeout');
}

async function startUpstream() {
  const socket = dgram.createSocket('udp4');
  const held = new Map(); let invalid = 0; let received = 0;
  socket.on('error', () => { invalid++; });
  socket.on('message', (packet, peer) => {
    try {
      parse(packet); received++;
      const name = nameLabels(packet, 12).map(label => label.toString('ascii')).join('.');
      const hold = held.get(name);
      const reply = () => socket.send(hold ? cnameResponse(packet, hold.target) : aResponse(packet), peer.port, peer.address);
      if (hold) { assert(!hold.release, 'one held request'); hold.release = reply; hold.ready(); }
      else setTimeout(reply, 1);
    } catch { invalid++; }
  });
  await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
  return { port: socket.address().port, close: () => socket.close(),
    counts: () => ({ received, invalid }),
    hold(name, target) {
      let ready; const arrived = new Promise(resolve => { ready = resolve; });
      const entry = { target, ready, release: null }; held.set(name, entry);
      return { arrived, release() { assert(entry.release, 'held query reached upstream'); entry.release(); held.delete(name); } };
    } };
}
async function sentinel(port, label) {
  const blockedName = `lifecycle-${label.toLowerCase()}.test`;
  const otherName = `lifecycle-${label === 'A' ? 'b' : 'a'}.test`;
  const explanation = await request('POST', '/api/filter/check', { name: blockedName });
  assert.equal(explanation.decision, 'blocked');
  assert.equal(explanation.witness?.source_id, SOURCES[1].id);
  await probe(port, blockedName, true); await probe(port, otherName, false);
}
function configText(upstreamPort, enabled, withSources, round) {
  assert(Number.isInteger(round) && round >= 1 && round <= 3);
  return `listen='127.0.0.1:0'
query_timeout_ms=${SLO.server_deadline_ms}
tcp_io_timeout_ms=1000
shutdown_grace_ms=1000
max_inflight=4096
max_tcp_connections=32
[upstreams]
servers=['udp://127.0.0.1:${upstreamPort}']
[source_limits]
enabled=false
[ecs]
enabled=false
[cache]
enabled=true
max_entries=16384
max_bytes=67108864
shards=4
[cache.prefetch]
enabled=false
[cache.stale]
enabled=false
[cache.persistence]
enabled=false
[coalescing]
enabled=false
[query_log]
enabled=false
[updates]
auto_check=false
[filter]
enabled=true
block_exact=['local-round-${round}.lifecycle.test']
block_suffix=['local.lifecycle.test']
allow_exact=['allow.local.lifecycle.test']
[filter_subscriptions]
enabled=${enabled}
max_rules=1000000
max_memory_bytes=268435456
max_disk_bytes=268435456
${withSources ? SOURCES.map(source => `[[filter_subscriptions.sources]]
id='${source.id}'
name='${source.id}'
url='${source.url}'
format='domain_list'
enabled=true
auto_update=false
update_interval_hours=24
`).join('') : 'sources=[]\n'}`;
}
async function configure(upstreamPort, enabled, withSources, round) {
  const previous = await request('GET', '/api/config');
  const toml = configText(upstreamPort, enabled, withSources, round);
  await request('POST', '/api/config/validate', { toml });
  await request('PUT', '/api/config', { revision: previous.revision, toml });
  const current = await request('GET', '/api/config');
  assert.equal(current.toml, toml); assert.equal(current.revision, previous.revision + 1);
  const status = await request('GET', '/api/status');
  assert.equal(status.running, true); assert.equal(status.last_error, null);
  const match = /^127\.0\.0\.1:(\d+)$/.exec(status.listen); assert(match);
  return { revision: current.revision, port: Number(match[1]) };
}

async function resources(pid, cgroup) {
  const [usage, status, peak, state, io] = await Promise.all([
    linuxUsage(cgroup), readFile(`/proc/${pid}/status`, 'utf8'),
    readFile(path.join(cgroup, 'memory.peak'), 'utf8'), snapshot(), readFile(path.join(cgroup, 'io.stat'), 'utf8'),
  ]);
  const number = name => { const value = status.match(new RegExp(`^${name}:\\s*(\\d+) kB$`, 'm')); assert(value); return Number(value[1]) * 1024; };
  return { at_ms: performance.now(), ...usage, rss_bytes: number('VmRSS'),
    process_lifetime_rss_peak_bytes: number('VmHWM'), cgroup_lifetime_peak_bytes: Number(peak),
    block_io_by_device: ioStat(io), ...selected(state) };
}
function mainPid() {
  return Number(execFileSync('systemctl', ['show', '--property=MainPID', '--value', UNIT], { encoding: 'utf8' }).trim());
}
function privileged(pid, ...args) {
  // Rename existing inodes, never create an owner-dependent directory. If startup
  // failed and its namespace vanished, root can restore the same exact host path.
  assert(Number.isSafeInteger(pid) && pid >= 0);
  return execFileSync('sudo', ['-n', ...(pid > 1 ? ['nsenter', `--target=${pid}`, '--mount', '--root', '--'] : []), ...args],
    { encoding: 'utf8', maxBuffer: MiB, timeout: 10000, stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, LC_ALL: 'C' } }).trim();
}
function indexNames(pid) {
  const names = privileged(pid, 'find', `${FILTER_DIR}/indexes`, '-mindepth', '1', '-maxdepth', '1', '-printf', '%f\n').split('\n').filter(Boolean);
  names.forEach(name => assert.match(name, /^[a-f0-9]{64}\.bin$/)); return names;
}
function fileIdentity(pid, file) {
  const stat = privileged(pid, 'stat', '--format=%F|%h|%s|%d|%i|%Y', '--', file).split('|');
  assert.equal(stat[0], 'regular file'); assert.equal(stat[1], '1'); assert.match(stat[2], /^\d+$/);
  const sha256 = privileged(pid, 'sha256sum', '--', file).split(/\s+/)[0]; assert.match(sha256, /^[a-f0-9]{64}$/);
  stat.slice(3).forEach(value => assert.match(value, /^\d+$/));
  return { bytes: Number(stat[2]), sha256, device: stat[3], inode: stat[4], mtime_secs: Number(stat[5]) };
}
function persistentSources(pid, state) {
  const catalog = JSON.parse(privileged(pid, 'cat', `${FILTER_DIR}/catalog.json`));
  assert.equal(catalog.content_revision, state.content_revision);
  return state.sources.map(source => {
    const record = catalog.records.find(r => r.fingerprint === source.fingerprint); assert(record);
    assert.match(record.sha256, /^[a-f0-9]{64}$/);
    assert.equal(fileIdentity(pid, `${FILTER_DIR}/objects/${record.sha256}.txt`).sha256, record.sha256);
    return { id: source.id, sha256: record.sha256, rules: record.rules, last_attempt: source.last_attempt, last_success: source.last_success };
  });
}
async function startupSample({ round, mode, pid, indexName, current, fixture, output, serverCpus, driverCpus }) {
  assert.match(indexName, /^[a-f0-9]{64}\.bin$/); assert(['derived', 'raw'].includes(mode));
  const file = `${FILTER_DIR}/indexes/${indexName}`, backup = `${FILTER_DIR}/lifecycle-backup-r${round}.bin`;
  const before = await snapshot(), identity = fileIdentity(pid, file), sources = persistentSources(pid, before);
  const result = { round, mode, index_name: indexName, index_identity: identity, before: selected(before), sources, failures: [] };
  let moved = false, cgroup, failure;
  try {
    if (mode === 'raw') {
      assert.equal(privileged(pid, 'find', FILTER_DIR, '-maxdepth', '1', '-name', path.basename(backup), '-printf', '%f'), '');
      privileged(pid, 'mv', '--', file, backup); moved = true;
      assert.deepEqual(fileIdentity(pid, backup), identity);
      assert(!indexNames(pid).includes(indexName), 'selected derived index is absent');
    }
    execFileSync('sudo', ['-n', 'systemctl', 'stop', UNIT], { timeout: 150000, stdio: ['ignore', 'pipe', 'pipe'] });
    auth = undefined; const started = performance.now();
    execFileSync('sudo', ['-n', 'systemctl', 'start', UNIT], { timeout: 30000, stdio: ['ignore', 'pipe', 'pipe'] });
    pid = mainPid(); assert(pid > 1); await waitForManagement();
    await request('POST', '/api/login', JSON.parse(await readFile(path.join(fixture, 'credentials.json'), 'utf8')));
    let status;
    do {
      status = await request('GET', '/api/status');
      if (status.running) break;
      await delay(100);
    } while (performance.now() - started < SLO.startup_ms);
    assert.equal(status.running, true); assert.equal(status.last_error, null);
    const match = /^127\.0\.0\.1:(\d+)$/.exec(status.listen); assert(match);
    await sentinel(Number(match[1]), current);
    const after = await snapshot(); assert.equal(after.content_revision, before.content_revision);
    assert.equal(after.config_revision, before.config_revision); assert.equal(after.input_rules, before.input_rules);
    assert.deepEqual(selected(after).sources, selected(before).sources);
    result.startup_observed_ms = performance.now() - started;
    result.constraints = await verifyLinuxServer(pid, UNIT, serverCpus, driverCpus, 2147483648);
    cgroup = result.constraints.hierarchy[0].directory; result.resources = await resources(pid, cgroup);
    const stat = (await readFile(`/proc/${pid}/stat`, 'utf8')).split(') ').slice(1).join(') ').split(' ');
    const ticks = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' })); assert(ticks > 0);
    result.process_cpu_seconds = (Number(stat[11]) + Number(stat[12])) / ticks;
    result.process_io = Object.fromEntries(privileged(0, 'cat', `/proc/${pid}/io`).split('\n')
      .map(line => line.split(/:\s*/)).filter(([key]) => ['rchar', 'wchar', 'syscr', 'syscw', 'read_bytes', 'write_bytes'].includes(key))
      .map(([key, value]) => { assert.match(value, /^\d+$/); return [key, Number(value)]; }));
    assert.deepEqual(persistentSources(pid, after), sources, 'selected source bytes and download timestamps unchanged');
    const rebuilt = fileIdentity(pid, file); result.index_after = rebuilt;
    assert.equal(rebuilt.sha256, identity.sha256); assert.equal(rebuilt.bytes, identity.bytes);
    if (mode === 'derived') assert.deepEqual(rebuilt, identity, 'derived restart must not rewrite the selected index');
    else assert.notEqual(rebuilt.inode, identity.inode, 'raw startup must create a new index while the old inode remains backed up');
    result.after = selected(after); result.temporary_backup_bytes = moved ? identity.bytes : 0;
    if (result.startup_observed_ms > SLO.startup_ms) result.failures.push('startup_deadline');
    if (result.resources.process_lifetime_rss_peak_bytes > SLO.startup_rss_bytes) result.failures.push('startup_rss');
    if (['oom', 'oom_kill', 'oom_group_kill'].some(key => (result.resources.memory_events[key] ?? 0) !== 0)) result.failures.push('startup_oom');
  } catch (error) { failure = error; result.error = safeCode(error.code); result.http = error.http; }
  finally {
    if (moved) {
      try {
        const restorePid = mainPid(); assert.deepEqual(fileIdentity(restorePid, backup), identity);
        if (indexNames(restorePid).includes(indexName)) {
          const target = fileIdentity(restorePid, file);
          assert.equal(target.sha256, identity.sha256, 'preserve a mismatching rebuild alongside the recovery backup');
          assert.equal(target.bytes, identity.bytes);
        }
        privileged(restorePid, 'mv', '--', backup, file);
        assert.deepEqual(fileIdentity(restorePid, file), identity); result.backup_restored = true;
      } catch (error) { result.restore_error = safeCode(error.code); result.recovery_backup_path = backup; failure ??= error; }
    }
  }
  result.result = failure || result.failures.length ? 'failed' : 'passed';
  await writeFile(path.join(output, `r${round}-startup-${mode}.json`), JSON.stringify(result, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  console.log(JSON.stringify({ event: 'startup_complete', round, mode, result: result.result, startup_ms: result.startup_observed_ms, failures: result.failures }));
  if (failure) { failure.startup_sample = result; throw failure; }
  return { result, pid, cgroup };
}
async function phase({ active, pid, cgroup, port, onUpdate, ...args }) {
  return runWirePhase({ ...args, port, slo: SLO, actionKey: 'update', safeCode,
    errorDiagnostic: error => error.http,
    warmup: () => probe(port, 'fresh.lifecycle.test', false),
    readMetrics: async () => (await request('GET', '/api/status')).metrics.counters,
    observeResources: () => resources(pid, cgroup),
    itemFor: (sequence, name) => itemFor(sequence, name, active), onAction: onUpdate });
}

async function updateEvidence({ round, current, next, runtime, upstream, evidence }) {
  const before = await snapshot(); const name = `held-r${round}.lifecycle.test`;
  const attempt = { round, expected_sha256: digest(CONTENT[next]), before: selected(before),
    product_http_status: 'not_exposed_by_api' };
  evidence.push(attempt);
  const hold = upstream.hold(name, `lifecycle-${current.toLowerCase()}.test`);
  const heldStarted = performance.now();
  // Attach rejection immediately: a failed held request must never be unhandled.
  let heldFailure; const held = probe(runtime.port, name, true, SLO.server_deadline_ms)
    .catch(error => { heldFailure = error; });
  let released = false;
  try {
    await Promise.race([hold.arrived, delay(1000).then(() => { throw fail('held_upstream_not_reached'); })]);
    const started = performance.now();
    const refreshed = await operation('/api/filter/subscriptions/refresh',
      { config_revision: runtime.revision, source_id: SOURCES[1].id }, SLO.server_deadline_ms - 1000,
      (op, state) => {
        attempt.operation_id = op.id; attempt.operation_status = op.status;
        attempt.actual_sha256 = op.sha256 ?? null;
        if (op.error) attempt.operation_error = safeCode(op.error.code);
        if (state) attempt.after = selected(state);
      });
    if (refreshed.op.sha256 !== digest(CONTENT[next])) throw fail('fixture_not_ready');
    const pinned = await snapshot();
    assert(pinned.generation > before.generation); assert(pinned.content_revision > before.content_revision);
    assert(pinned.input_rules >= 200000); assert.equal(pinned.input_rules, before.input_rules);
    await sentinel(runtime.port, next);
    const readyMs = performance.now() - started;
    assert(pinned.retained_bytes > before.retained_bytes, 'old generation retained during publication');
    const refused = await request('POST', '/api/filter/subscriptions/refresh',
      { config_revision: runtime.revision, source_id: SOURCES[0].id }, 409);
    assert.equal(refused.error?.code, 'busy', 'no second compilation while old reader is held');
    hold.release(); released = true; await held;
    if (heldFailure) throw heldFailure;
    assert(performance.now() - heldStarted < SLO.server_deadline_ms, 'held query crosses publication before deadline');
    const until = performance.now() + 2000; let after;
    do { after = await snapshot(); if (after.retained_bytes < pinned.retained_bytes) break; await delay(20); } while (performance.now() < until);
    assert.equal(after.generation, pinned.generation);
    assert(after.retained_bytes < pinned.retained_bytes, 'old generation released');
    return { previous: current, next, source_sha256: refreshed.op.sha256,
      operation_id: refreshed.op.id, operation_observed_ms: refreshed.observed_ms,
      full_update_observed_ms: readyMs, timing_scope: 'refresh request through terminal operation and new-generation DNS checks; 100ms polling upper bound, not compile time',
      held_request_ms: performance.now() - heldStarted, second_compile_rejected: true,
      before: selected(before), held: selected(pinned), released: selected(after) };
  } catch (error) { attempt.error = safeCode(error.code); throw error; }
  finally {
    if (!released) { try { hold.release(); } catch { /* No upstream arrival; held request is bounded. */ } }
    await held;
  }
}

async function run(fixture, output) {
  assert(process.platform === 'linux' && process.env.GITHUB_ACTIONS === 'true'
    && process.env.RUNNER_ENVIRONMENT === 'github-hosted' && process.env.RUNNER_OS === 'Linux',
  'requires a disposable GitHub Linux runner');
  assert(path.isAbsolute(fixture) && path.isAbsolute(output));
  const serverCpus = cpuList(process.env.PARINS_FS_SERVER_CPUS);
  const driverCpus = cpuList(process.env.PARINS_FS_DRIVER_CPUS); assert.equal(serverCpus.length, 2);
  let pid = mainPid();
  assert(Number.isSafeInteger(pid) && pid > 1);
  const constraints = await verifyLinuxServer(pid, UNIT, serverCpus, driverCpus, 2147483648);
  let cgroup = constraints.hierarchy[0].directory;
  await mkdir(output, { recursive: true, mode: 0o700 });
  const summary = { result: 'running', started_at: new Date().toISOString(), slo: SLO,
    source_urls: SOURCES.map(s => s.url), corpus_sha256: CORPUS_SHA,
    mutable_sha256: Object.fromEntries(Object.entries(CONTENT).map(([key, value]) => [key, digest(value)])),
    scope: 'installed managed service; 2 CPUs / 2 GiB / no swap; UDP loopback mock 1ms; source_limits disabled only in fixture; no public-network/protocol-mix capacity claim',
    memory_scope: 'steady RSS sampled at 100ms; update VmHWM is conservative process-lifetime RSS peak; cgroup memory.peak includes file cache and is reported separately',
    deadlines: 'server 20000ms permits one held-generation correctness probe; every load sample uses 1000ms from its planned send time; held probe excluded from SLO',
    compilation_scope: 'one unqueried local-round-N.lifecycle.test block_exact marker is fixed across each round baseline/steady/update and differs between rounds; each updated aggregate material digest is new, preventing A/B derived-index reuse across rounds',
    startup_scope: 'full managed-process stop/start with existing selected sources; no OS page-cache flush or physical cold-disk claim; normal network remains available, source SHA and download timestamps must be unchanged; raw mode temporarily renames only the selected derived index and restores its backup in finally',
    startup_accounting: 'wall time includes start command, read-only readiness, one login, DNS probes and sanitized state checks; process CPU/IO and lifetime RSS measured after readiness; cgroup io.stat is per-device and may include earlier unit work; raw snapshot disk_bytes includes the temporary backup and may remain cached after its removal',
    constraints, phases: [], startups: [], readiness: [], refresh_attempts: [] };
  const journal = JSON.parse(execFileSync('sudo', ['-n', 'cat', '/var/lib/parins-updater/private/journal.json'],
    { maxBuffer: MiB, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }));
  assert.match(journal.installed.build.source_commit, /^[a-f0-9]{40}$/);
  summary.app_source_commit = journal.installed.build.source_commit;
  summary.fixture_sha256 = digest(await readFile(new URL(import.meta.url)));
  const artifact = path.join(output, 'summary.json');
  await writeFile(artifact, JSON.stringify(summary, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  let upstream;
  try {
    await waitForManagement();
    await request('POST', '/api/login', JSON.parse(await readFile(path.join(fixture, 'credentials.json'), 'utf8')));
    const initial = await request('GET', '/api/config');
    assert(!initial.toml.includes('[filter_subscriptions]') && !initial.toml.includes('[updates]'), 'fresh installer fixture required');
    await waitForInstallation();
    upstream = await startUpstream(); let current = 'A'; let prepared = false;
    for (let round = 1; round <= 3; round++) {
      let runtime = await configure(upstream.port, false, prepared, round);
      const args = { port: runtime.port, pid, cgroup, output };
      summary.phases.push(await phase({ ...args, name: `r${round}-baseline`, active: false, duration: SLO.steady_ms }));
      if (!prepared) {
        await sourceReady(0, 'A', null, summary.readiness);
        for (const source of SOURCES) {
          const result = await operation('/api/filter/subscriptions/prepare', { config_revision: runtime.revision, source });
          assert.equal(result.op.sha256, source.id === SOURCES[0].id ? CORPUS_SHA : digest(CONTENT.A));
          assert.equal(result.op.rules, source.id === SOURCES[0].id ? 201337 : 2);
        }
        prepared = true;
      }
      runtime = await configure(upstream.port, true, true, round);
      await sentinel(runtime.port, current);
      const active = await snapshot(); assert(active.input_rules >= 200000);
      assert(active.sources.every(s => s.active && s.ready));
      summary.phases.push(await phase({ ...args, port: runtime.port, name: `r${round}-steady`, active: true, duration: SLO.steady_ms }));
      const next = current === 'A' ? 'B' : 'A';
      const state = await snapshot();
      const due = (state.sources.find(s => s.id === SOURCES[1].id).last_attempt ?? 0) * 1000 + 61000;
      while (Date.now() < due) await delay(Math.min(1000, due - Date.now()));
      await sourceReady(round, next, sourceBinding(pid, await snapshot(), current), summary.readiness);
      const indexesBefore = indexNames(pid);
      const update = await phase({ ...args, port: runtime.port, name: `r${round}-update`, active: true, duration: SLO.update_ms,
        onUpdate: () => updateEvidence({ round, current, next, runtime, upstream, evidence: summary.refresh_attempts }) });
      summary.phases.push(update);
      if (!update.update) throw fail('update_evidence_failed');
      current = next;
      const newIndexes = indexNames(pid).filter(name => !indexesBefore.includes(name));
      assert.equal(newIndexes.length, 1, 'one new derived index belongs to the observed successful publication');
      update.update.derived_index_name = newIndexes[0];
      await writeFile(path.join(output, `${update.name}.json`), JSON.stringify(update, null, 2) + '\n', { mode: 0o600 });
      for (const mode of ['derived', 'raw']) {
        const startup = await startupSample({ round, mode, pid, indexName: newIndexes[0], current, fixture, output, serverCpus, driverCpus });
        summary.startups.push(startup.result); pid = startup.pid; cgroup = startup.cgroup;
      }
      await writeFile(artifact, JSON.stringify(summary, null, 2) + '\n', { mode: 0o600 });
    }
    summary.upstream = upstream.counts(); assert.equal(summary.upstream.invalid, 0);
    summary.constraints_after = await verifyLinuxServer(pid, UNIT, serverCpus, driverCpus, 2147483648);
    summary.result = [...summary.phases, ...summary.startups].every(p => p.result === 'passed') ? 'passed' : 'failed';
    if (summary.result !== 'passed') process.exitCode = 1;
  } catch (error) {
    if (error.startup_sample) summary.startups.push(error.startup_sample);
    summary.result = 'failed'; summary.error = safeCode(error.code); summary.http = error.http;
    summary.location = String(error.stack).match(/test-filter-lifecycle\.mjs:\d+:\d+/)?.[0] ?? 'unknown';
    process.exitCode = 1;
  } finally {
    upstream?.close(); summary.finished_at = new Date().toISOString();
    await writeFile(artifact, JSON.stringify(summary, null, 2) + '\n', { mode: 0o600 });
    console.log(JSON.stringify({ event: 'lifecycle_complete', result: summary.result,
      error: summary.error, location: summary.location, http: summary.http, phases: summary.phases.length }));
  }
}

function selfTest() {
  assert.deepEqual(distribution([3, 1, 2, 100]), { count: 4, p50: 2, p95: 100, p99: 100, max: 100 });
  assert.equal(distribution([]).p99, null);
  assert.equal(plannedCount(60000, 1000), 60000); assert.equal(plannedCount(120000, 1000), 120000);
  assert.equal(dueCount(-1, 60000, 1000), 0); assert.equal(dueCount(0, 60000, 1000), 1);
  assert.equal(dueCount(59999, 60000, 1000), 60000); assert.equal(dueCount(61000, 60000, 1000), 60000);
  assert.equal(dueCount(123.9, 60000, 1000), 124, 'late driver catches up to absolute schedule');
  assert.equal(59940 / 60, SLO.correct_qps); assert.equal(60 / 60000, SLO.timeout_fraction);
  assert.equal(itemFor(2, 'test', false).blocked, false); assert.equal(itemFor(2, 'test', true).blocked, true);
  const packet = query(42, 'fresh.lifecycle.test'); const response = aResponse(packet);
  oracle(response, packet, false);
  for (const mutate of [b => b.writeUInt16BE(43, 0), b => b.writeUInt16BE(0x8380, 2), b => { b[b.length - 1] = 99; },
    b => b.writeUInt32BE(0, b.length - 10), b => { b[13] ^= 1; }]) {
    const bad = Buffer.from(response); mutate(bad); assert.throws(() => oracle(bad, packet, false));
  }
  assert.throws(() => oracle(response, packet, true));
  const nodata = Buffer.from(packet); nodata.writeUInt16BE(0x8180, 2); oracle(nodata, packet, true);
  const cname = parse(cnameResponse(packet, 'lifecycle-a.test')); assert.equal(cname.answers.length, 2);
  assert.equal(cname.answers[0].type, 5); assert.equal(cname.answers[1].type, 1);
  assert.notEqual(digest(CONTENT.A), digest(CONTENT.B));
  const binding = { etag: '"fixture-a"', last_modified: 'Wed, 07 Oct 2026 00:00:00 GMT', content_sha256: digest(CONTENT.A) };
  assert.deepEqual(sourceHeaders(null), { 'User-Agent': 'PariNS-subscriptions/1', Accept: 'text/plain',
    'Accept-Encoding': 'gzip, identity', Connection: 'close' });
  assert.equal(sourceHeaders(binding)['If-None-Match'], binding.etag);
  assert.equal(sourceHeaders(binding)['If-Modified-Since'], undefined);
  assert.equal(sourceHeaders({ ...binding, etag: null })['If-Modified-Since'], binding.last_modified);
  assert.deepEqual(sourceConditional(binding), { kind: 'etag', validator_sha256: digest(binding.etag), content_sha256: digest(CONTENT.A) });
  assert.deepEqual(sourceConditional(null), { kind: null, validator_sha256: null, content_sha256: null });
  assert.equal(sourceBodySha(304, undefined, Buffer.alloc(0)), null);
  assert.equal(sourceBodySha(200, undefined, Buffer.from(CONTENT.A)), digest(CONTENT.A));
  assert.equal(sourceBodySha(200, 'gzip', gzipSync(CONTENT.B)), digest(CONTENT.B));
  assert.throws(() => sourceBodySha(200, 'br', Buffer.from(CONTENT.A)));
  assert.throws(() => sourceBodySha(200, 'gzip', gzipSync(Buffer.alloc(4097))));
  assert.throws(() => sourceBodySha(200, 'identity', Buffer.alloc(4097)));
  assert.match(configText(1234, true, true, 1), /query_timeout_ms=20000/);
  for (const source of SOURCES) {
    assert(configText(1234, true, true, 1).includes(`id='${source.id}'\nname='${source.id}'`));
    assert.deepEqual(Object.keys(source).sort(), ['format', 'id', 'url'], 'prepare uses only DraftSource fields');
  }
  assert.deepEqual(httpDiagnostic('POST', '/api/config/validate', 422, 200, 'INVALID_CONFIG'),
    { method: 'POST', api_path: '/api/config/validate', actual_status: 422, expected_status: 200, api_error_code: 'INVALID_CONFIG' });
  assert.equal(httpDiagnostic('POST', '/untrusted', 422, 200, 'secret'), undefined);
  assert.notEqual(configText(1234, true, true, 1), configText(1234, true, true, 2));
  assert.deepEqual(ioStat('8:0 rbytes=12 wbytes=34 rios=1 wios=2 dbytes=99\n'), { '8:0': { rbytes: 12, wbytes: 34, rios: 1, wios: 2 } });
  assert.deepEqual(ioStat(''), {}); assert.throws(() => ioStat('bad rbytes=1'));
  console.log('filter lifecycle self-test passed');
}

if (process.argv.length === 3 && process.argv[2] === '--self-test') selfTest();
else {
  try {
    assert.equal(process.argv.length, 4, 'usage: node scripts/test-filter-lifecycle.mjs <private fixture dir> <output dir>');
    await run(process.argv[2], process.argv[3]);
  } catch (error) {
    console.error(JSON.stringify({ event: 'lifecycle_refused', code: safeCode(error.code) }));
    process.exitCode = 1;
  }
}

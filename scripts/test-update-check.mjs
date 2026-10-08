// Real official-A metadata checks only. No apply, candidate download or release fabrication.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { cpuList, linuxUsage, parse, query, verifyLinuxServer } from './lib/wire-bench.mjs';
import { aResponse, distribution, dueCount, ioStat, oracle, plannedCount, probe, runWirePhase } from './lib/managed-wire.mjs';

const MiB = 1024 * 1024;
// The shared driver's update_* names mean the caller's action window here: CHECK.
const SLO = Object.freeze({ rate: 1000, steady_ms: 60000, update_ms: 120000,
  refresh_at_ms: 15000, client_deadline_ms: 1000, steady_p99_ms: 20, update_p99_ms: 50,
  timeout_fraction: .001, correct_qps: 999, steady_rss_bytes: 256 * MiB, update_rss_bytes: 512 * MiB });
const base = 'http://127.0.0.1:3000';
const UNIT = 'parins-managed.service';
const LATEST = 'https://api.github.com/repos/paricafe/PariNS/releases/latest';
const safeCode = value => typeof value === 'string' && /^[A-Za-z0-9_]{1,64}$/.test(value) ? value : 'fixture_failed';
const fail = code => Object.assign(new Error(code), { code });
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
let auth;
function httpDiagnostic(method, endpoint, actual, expected, code) {
  if (!['GET', 'POST', 'PUT'].includes(method)
    || !['/api/session', '/api/login', '/api/config', '/api/config/validate', '/api/status', '/api/updates', '/api/updates/check'].includes(endpoint)
    || !Number.isInteger(actual) || actual < 100 || actual > 599) return undefined;
  return { method, api_path: endpoint, actual_status: actual, expected_status: expected, api_error_code: safeCode(code) };
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
            error.http = httpDiagnostic(method, endpoint, res.statusCode, expected, value.error?.code); throw error;
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
const updates = () => request('GET', '/api/updates');
async function ready() {
  const deadline = performance.now() + 30000;
  while (performance.now() < deadline) {
    try {
      const state = await request('GET', '/api/session', undefined, 200, 1000);
      assert.equal(state.setup_required, false); assert.equal(state.authenticated, false); assert.equal(state.session, null); return;
    } catch (error) {
      if (!['ECONNREFUSED', 'ECONNRESET', 'api_read_timeout'].includes(error.code)) throw error;
    }
    await delay(250);
  }
  throw fail('management_readiness_timeout');
}
function expectedBuild(build, version, commit) {
  assert.equal(build.official_release, true); assert.equal(build.version, version);
  assert.equal(build.source_commit, commit); assert.equal(build.target, 'x86_64-unknown-linux-musl');
}
function supported(state, version, commit) {
  expectedBuild(state.current, version, commit);
  assert.equal(state.capability.available, true); assert.equal(state.capability.reason, null);
  assert.equal(state.frozen, false); assert.equal(state.active_operation, null);
}
function selectedCheck(state) {
  return { state: safeCode(state.check.state), last_check_at_ms: state.check.last_check_at_ms,
    last_success_at_ms: state.check.last_success_at_ms, retry_at_ms: state.check.retry_at_ms,
    error: state.check.error === null ? null : safeCode(state.check.error),
    candidate_present: state.candidate !== null, frozen: state.frozen,
    active_operation_present: state.active_operation !== null };
}
async function quiet(version, commit, honorCooldown) {
  const began = performance.now(); let previous, stableSince = began;
  while (performance.now() - began < 120000) {
    const state = await updates(); expectedBuild(state.current, version, commit);
    if (!state.capability.available && state.capability.reason === 'installation_pending') { await delay(250); continue; }
    supported(state, version, commit);
    assert(Number.isSafeInteger(state.check.retry_at_ms) && state.check.retry_at_ms >= 0);
    const key = JSON.stringify(selectedCheck(state));
    if (key !== previous || state.check.state === 'checking') stableSince = performance.now();
    previous = key;
    // The process-owned scheduler revisits saved settings at least once per second.
    // An additional idle-phase check below rejects any late pre-existing check.
    if (state.check.state !== 'checking' && performance.now() - stableSince >= 1500
      && (!honorCooldown || Date.now() >= state.check.retry_at_ms)) return state;
    await delay(250);
  }
  throw fail('updater_not_quiet_or_cooldown_pending');
}
function releaseIdentity(release, version) {
  assert.equal(release.tag_name, `v${version}`); assert.equal(release.draft, false); assert.equal(release.prerelease, false);
  assert.equal(release.html_url, `https://github.com/paricafe/PariNS/releases/tag/v${version}`);
  assert(Number.isSafeInteger(release.id) && release.id > 0);
  assert(typeof release.published_at === 'string' && Number.isFinite(Date.parse(release.published_at)));
  return { tag: release.tag_name, release_id: release.id, published_at: release.published_at };
}
async function publicLatest(version) {
  // Intentionally unauthenticated: management cookies/bindings never enter fetch.
  const response = await fetch(LATEST, { redirect: 'error', signal: AbortSignal.timeout(15000),
    headers: { Accept: 'application/vnd.github+json', 'User-Agent': 'PariNS-official-check-fixture' } });
  if (!response.ok) throw fail(`public_latest_http_${response.status}`);
  const reader = response.body.getReader(); let bytes = Buffer.alloc(0);
  while (true) {
    const chunk = await reader.read(); if (chunk.done) break;
    bytes = Buffer.concat([bytes, chunk.value]);
    if (bytes.length > MiB) { await reader.cancel(); throw fail('public_latest_body_limit'); }
  }
  return releaseIdentity(JSON.parse(bytes), version);
}
function installationIdentity(pid, version, commit) {
  const journal = JSON.parse(execFileSync('sudo', ['-n', 'cat', '/var/lib/parins-updater/private/journal.json'],
    { maxBuffer: MiB, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }));
  assert.equal(journal.installation_pending, null); expectedBuild(journal.installed.build, version, commit);
  const executable = execFileSync('sudo', ['-n', 'sha256sum', `/proc/${pid}/exe`],
    { maxBuffer: 1024, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).split(/\s+/)[0];
  assert.match(executable, /^[a-f0-9]{64}$/); assert.equal(journal.installed.sha256, executable);
  return { build: journal.installed.build, executable_sha256: executable };
}
function workload(sequence, phase) {
  switch (sequence % 4) {
    case 0: return { group: 'fresh_cache', name: 'fresh.check.test', blocked: false };
    case 1: return { group: 'unique_miss', name: `m${phase}-${sequence}.check.test`, blocked: false };
    case 2: return { group: 'blocked_local', name: 'blocked.check.test', blocked: true };
    default: return { group: 'allow_local', name: 'allow.local.check.test', blocked: false };
  }
}
async function startUpstream() {
  const socket = dgram.createSocket('udp4'); let received = 0, invalid = 0;
  socket.on('error', () => { invalid++; });
  socket.on('message', (packet, peer) => {
    try { parse(packet); received++; setTimeout(() => socket.send(aResponse(packet), peer.port, peer.address), 1); }
    catch { invalid++; }
  });
  await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
  return { port: socket.address().port, counts: () => ({ received, invalid }), close: () => socket.close() };
}
function configText(port) {
  return `listen='127.0.0.1:0'
query_timeout_ms=1000
tcp_io_timeout_ms=1000
shutdown_grace_ms=1000
max_inflight=4096
max_tcp_connections=32
[upstreams]
servers=['udp://127.0.0.1:${port}']
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
block_exact=['blocked.check.test']
block_suffix=['local.check.test']
allow_exact=['allow.local.check.test']
[filter_subscriptions]
enabled=false
sources=[]
`;
}
async function configure(port) {
  const previous = await request('GET', '/api/config'), toml = configText(port);
  await request('POST', '/api/config/validate', { toml });
  await request('PUT', '/api/config', { revision: previous.revision, toml });
  const current = await request('GET', '/api/config');
  assert.equal(current.toml, toml); assert.equal(current.revision, previous.revision + 1);
  const status = await request('GET', '/api/status'); assert.equal(status.running, true); assert.equal(status.last_error, null);
  const match = /^127\.0\.0\.1:(\d+)$/.exec(status.listen); assert(match);
  return { revision: current.revision, port: Number(match[1]) };
}
async function observeResources(pid, cgroup) {
  const [usage, status, peak, io, update] = await Promise.all([linuxUsage(cgroup), readFile(`/proc/${pid}/status`, 'utf8'),
    readFile(path.join(cgroup, 'memory.peak'), 'utf8'), readFile(path.join(cgroup, 'io.stat'), 'utf8'), updates()]);
  assert.equal(update.frozen, false); assert.equal(update.active_operation, null);
  const number = name => { const value = status.match(new RegExp(`^${name}:\\s*(\\d+) kB$`, 'm')); assert(value); return Number(value[1]) * 1024; };
  return { at_ms: performance.now(), ...usage, rss_bytes: number('VmRSS'), process_lifetime_rss_peak_bytes: number('VmHWM'),
    cgroup_lifetime_peak_bytes: Number(peak), block_io_by_device: ioStat(io), updater: selectedCheck(update) };
}
function completedNewCheck(state, before, requestedAt) {
  return state.check.state === 'up_to_date' && state.check.error === null && state.candidate === null
    && Number.isSafeInteger(state.check.last_check_at_ms) && state.check.last_check_at_ms >= requestedAt
    && state.check.last_check_at_ms > (before.check.last_check_at_ms ?? 0)
    && Number.isSafeInteger(state.check.last_success_at_ms)
    && state.check.last_success_at_ms > (before.check.last_success_at_ms ?? 0)
    && state.check.last_success_at_ms >= state.check.last_check_at_ms;
}
async function checkOnce(version, commit, evidence) {
  const before = await updates(); supported(before, version, commit);
  assert.notEqual(before.check.state, 'checking'); assert(Date.now() >= before.check.retry_at_ms);
  const event = { before: selectedCheck(before), requested_at_ms: Date.now(), result: 'running' }; evidence.push(event);
  const began = performance.now();
  try {
    const accepted = await request('POST', '/api/updates/check', {}, 202);
    assert.equal(accepted.accepted, true); assert.equal(accepted.status_url, '/api/updates'); event.accepted = true;
    while (performance.now() - began < 90000) {
      const state = await request('GET', '/api/updates', undefined, 200, Math.min(5000, Math.max(1, 90000 - (performance.now() - began))));
      supported(state, version, commit); event.after = selectedCheck(state);
      if (completedNewCheck(state, before, event.requested_at_ms)) {
        event.result = 'passed'; return event;
      }
      if (state.check.last_check_at_ms >= event.requested_at_ms && state.check.state === 'failed') throw fail('official_check_failed');
      await delay(100);
    }
    throw fail('official_check_completion_deadline');
  } catch (error) { event.result = 'failed'; event.error = safeCode(error.code); event.http = error.http; throw error; }
  finally { event.observed_action_ms = performance.now() - began; }
}
async function run(fixture, output) {
  assert(process.platform === 'linux' && process.env.GITHUB_ACTIONS === 'true'
    && process.env.RUNNER_ENVIRONMENT === 'github-hosted' && process.env.RUNNER_OS === 'Linux', 'requires disposable hosted Linux');
  assert(path.isAbsolute(fixture) && path.isAbsolute(output));
  const version = process.env.PARINS_EXPECTED_OFFICIAL_VERSION, commit = process.env.PARINS_EXPECTED_OFFICIAL_COMMIT;
  assert.equal(version, '0.1.5'); assert.match(commit ?? '', /^[a-f0-9]{40}$/);
  const serverCpus = cpuList(process.env.PARINS_FS_SERVER_CPUS), driverCpus = cpuList(process.env.PARINS_FS_DRIVER_CPUS);
  assert.equal(serverCpus.length, 2);
  const pid = Number(execFileSync('systemctl', ['show', '--property=MainPID', '--value', UNIT], { encoding: 'utf8' }).trim());
  assert(Number.isSafeInteger(pid) && pid > 1);
  const constraints = await verifyLinuxServer(pid, UNIT, serverCpus, driverCpus, 2147483648), cgroup = constraints.hierarchy[0].directory;
  await mkdir(output, { recursive: true, mode: 0o700 });
  const artifact = path.join(output, 'summary.json');
  const summary = { result: 'running', started_at: new Date().toISOString(), official_version: version, official_source_commit: commit,
    fixture_sha256: digest(await readFile(new URL(import.meta.url))), constraints, slo: SLO, phases: [], checks: [], latest: [],
    scope: 'actual official A idle and metadata-check only, three fixed rounds; subscriptions off; local UDP 1ms mock; source_limits disabled in fixture; no public-network capacity or B download/apply/rollback claim',
    timing_scope: 'check phase is a 120s surrounding window; observed_action_ms separately bounds POST through a new persisted success with 100ms polling, not 120s continuous check work',
    memory_scope: 'idle RSS sampled at 100ms; check uses conservative whole-process VmHWM; cgroup peak and per-device IO reported separately' };
  await writeFile(artifact, JSON.stringify(summary, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  let upstream;
  try {
    await ready(); await request('POST', '/api/login', JSON.parse(await readFile(path.join(fixture, 'credentials.json'), 'utf8')));
    await quiet(version, commit, false);
    upstream = await startUpstream(); const runtime = await configure(upstream.port);
    summary.initial_check = selectedCheck(await quiet(version, commit, true));
    const identity = installationIdentity(pid, version, commit); assert.deepEqual((await updates()).current, identity.build);
    summary.installed = { version, source_commit: commit, target: identity.build.target, official_release: true, executable_sha256: identity.executable_sha256 };
    const common = { port: runtime.port, output, slo: SLO, actionKey: 'check', safeCode, errorDiagnostic: error => error.http,
      warmup: () => probe(runtime.port, 'fresh.check.test', false), itemFor: workload,
      readMetrics: async () => (await request('GET', '/api/status')).metrics.counters,
      observeResources: () => observeResources(pid, cgroup) };
    for (let round = 1; round <= 3; round++) {
      summary.latest.push({ round, ...await publicLatest(version) });
      const idleBefore = await quiet(version, commit, true);
      const idle = await runWirePhase({ ...common, name: `r${round}-idle`, duration: SLO.steady_ms });
      const idleAfter = await updates(); supported(idleAfter, version, commit);
      idle.updater_before = selectedCheck(idleBefore); idle.updater_after = selectedCheck(idleAfter);
      if (idleAfter.check.last_check_at_ms !== idleBefore.check.last_check_at_ms
        || idle.resource_samples.some(sample => sample.updater.state === 'checking'
          || sample.updater.last_check_at_ms !== idleBefore.check.last_check_at_ms)) {
        idle.failures.push('background_check_during_idle'); idle.result = 'failed';
      }
      await writeFile(path.join(output, `${idle.name}.json`), JSON.stringify(idle, null, 2) + '\n', { mode: 0o600 }); summary.phases.push(idle);
      await quiet(version, commit, true);
      const checked = await runWirePhase({ ...common, name: `r${round}-check`, duration: SLO.update_ms,
        onAction: () => checkOnce(version, commit, summary.checks) }); summary.phases.push(checked);
      const status = await request('GET', '/api/status'); assert.equal(status.running, true); assert.equal(status.last_error, null);
      await writeFile(artifact, JSON.stringify(summary, null, 2) + '\n', { mode: 0o600 });
    }
    summary.latest.push({ stage: 'final', ...await publicLatest(version) });
    assert.deepEqual(installationIdentity(pid, version, commit), identity);
    assert.equal((await request('GET', '/api/config')).revision, runtime.revision);
    summary.constraints_after = await verifyLinuxServer(pid, UNIT, serverCpus, driverCpus, 2147483648);
    summary.upstream = upstream.counts(); assert.equal(summary.upstream.invalid, 0);
    summary.result = summary.phases.length === 6 && summary.checks.length === 3
      && summary.phases.every(phase => phase.result === 'passed') && summary.checks.every(check => check.result === 'passed') ? 'passed' : 'failed';
  } catch (error) {
    summary.result = 'failed'; summary.error = safeCode(error.code); summary.http = error.http;
    summary.location = String(error.stack).match(/test-update-check\.mjs:\d+:\d+/)?.[0] ?? 'unknown';
  } finally {
    upstream?.close(); summary.finished_at = new Date().toISOString();
    await writeFile(artifact, JSON.stringify(summary, null, 2) + '\n', { mode: 0o600 });
    console.log(JSON.stringify({ event: 'official_check_complete', result: summary.result, error: summary.error,
      http: summary.http, phases: summary.phases.length, checks: summary.checks.length }));
    if (summary.result !== 'passed') process.exitCode = 1;
  }
}
function selfTest() {
  assert.equal(plannedCount(60000, SLO.rate), 60000); assert.equal(plannedCount(120000, SLO.rate), 120000);
  assert.equal(dueCount(123.9, 60000, SLO.rate), 124); assert.equal(distribution([1, 2, 3, 100]).p99, 100);
  const packet = query(7, 'fresh.check.test'); oracle(aResponse(packet), packet, false);
  assert.throws(() => oracle(aResponse(packet), packet, true)); assert.equal(workload(2, 'test').blocked, true);
  const release = { tag_name: 'v0.1.5', draft: false, prerelease: false, id: 1, published_at: '2026-10-08T00:00:00Z', html_url: 'https://github.com/paricafe/PariNS/releases/tag/v0.1.5' };
  assert.equal(releaseIdentity(release, '0.1.5').tag, 'v0.1.5');
  assert.throws(() => releaseIdentity({ ...release, tag_name: 'v0.1.4' }, '0.1.5'));
  const build = { official_release: true, version: '0.1.5', source_commit: 'a'.repeat(40), target: 'x86_64-unknown-linux-musl' };
  const available = { current: build, capability: { available: true, reason: null }, frozen: false, active_operation: null };
  supported(available, '0.1.5', 'a'.repeat(40));
  assert.throws(() => supported({ ...available, capability: { available: false, reason: 'unsupported_installation' } }, '0.1.5', 'a'.repeat(40)));
  const before = { check: { last_check_at_ms: 100, last_success_at_ms: 110 } };
  const after = { check: { state: 'up_to_date', last_check_at_ms: 200, last_success_at_ms: 220, error: null }, candidate: null };
  assert(completedNewCheck(after, before, 190)); assert(!completedNewCheck(after, before, 210));
  assert(!completedNewCheck({ ...after, check: { ...after.check, last_success_at_ms: 110 } }, before, 190));
  assert(!completedNewCheck({ ...after, candidate: {} }, before, 190));
  assert.match(configText(5354), /\[filter_subscriptions\]\nenabled=false\nsources=\[\]/);
  assert.match(configText(5354), /\[updates\]\nauto_check=false/);
  assert.equal(httpDiagnostic('GET', '/untrusted', 422, 200, 'secret'), undefined);
  console.log('official update-check self-test passed');
}
if (process.argv.length === 3 && process.argv[2] === '--self-test') selfTest();
else {
  try { assert.equal(process.argv.length, 4); await run(process.argv[2], process.argv[3]); }
  catch (error) { console.error(JSON.stringify({ event: 'official_check_refused', code: safeCode(error.code) })); process.exitCode = 1; }
}

// Isolated UDP wire baseline against the production binary and loopback upstream.
import assert from 'node:assert/strict';
import { spawn, execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { finished } from 'node:stream/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { aResponse, distribution, probe, runWireLoad } from './lib/managed-wire.mjs';

const exec = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const kinds = ['direct', 'warm-cache', 'cold-unique', 'shared-miss', 'many-active'];
export function cpuSeconds(value) {
  const match = /^(?:(\d+)-)?(\d+):(\d{2})(?::(\d{2}(?:\.\d+)?))?$/.exec(value);
  // ps uses mm:ss[.xx] on macOS and [days-]hh:mm:ss on Linux.
  if (!match && /^\d+:\d{2}\.\d+$/.test(value)) {
    const [minutes, seconds] = value.split(':').map(Number); return minutes * 60 + seconds;
  }
  assert(match, `invalid ps CPU time: ${value}`);
  return Number(match[1] ?? 0) * 86400 + (match[4] === undefined
    ? Number(match[2]) * 60 + Number(match[3])
    : Number(match[2]) * 3600 + Number(match[3]) * 60 + Number(match[4]));
}
export function options(args) {
  const result = { seconds: 5, repeats: 3, rates: [1000, 4000], workers: 4, profile: false, kinds };
  for (let at = 0; at < args.length; at++) {
    const key = args[at];
    if (key === '--profile') { result.profile = true; continue; }
    assert(['--binary', '--output', '--seconds', '--repeats', '--rates', '--workers', '--cases'].includes(key), `unknown option: ${key}`);
    const value = args[++at]; assert(value && !value.startsWith('--'), `missing ${key}`);
    if (key === '--binary' || key === '--output') result[key.slice(2)] = path.resolve(value);
    else if (key === '--rates') result.rates = value.split(',').map(Number);
    else if (key === '--cases') { result.kinds = value.split(','); assert(result.kinds.every(k => kinds.includes(k)), 'unknown case'); }
    else result[key.slice(2)] = Number(value);
  }
  assert(result.binary && result.output, '--binary and --output are required');
  for (const [name, low, high] of [['seconds', 1, 30], ['repeats', 1, 10], ['workers', 1, 16]]) {
    assert(Number.isInteger(result[name]) && result[name] >= low && result[name] <= high, `invalid ${name}`);
  }
  assert(result.rates.length > 0 && result.rates.every(n => Number.isInteger(n) && n >= 1 && n <= 12000), 'rates must be 1..12000');
  assert(new Set(result.rates).size === result.rates.length && new Set(result.kinds).size === result.kinds.length, 'duplicate workload');
  assert(!result.profile || process.platform === 'darwin', '--profile requires the macOS sample tool');
  assert(!result.profile || result.seconds >= 5, '--profile needs at least 5 seconds');
  return result;
}
async function metrics(port) {
  const response = await fetch(`http://127.0.0.1:${port}/metrics`, { signal: AbortSignal.timeout(2000) });
  assert(response.ok, 'metrics HTTP status');
  return Object.fromEntries([...(await response.text()).matchAll(/^parins_([a-z_]+)_total (\d+)$/gm)].map(m => [m[1], Number(m[2])]));
}
async function resources(pid) {
  const { stdout } = await exec('ps', ['-p', String(pid), '-o', 'rss=,time='], { timeout: 2000 });
  const [rss, cpu] = stdout.trim().split(/\s+/);
  assert(/^\d+$/.test(rss), 'ps RSS');
  return { at_ms: Date.now(), rss_bytes: Number(rss) * 1024, cpu_seconds: cpuSeconds(cpu), driver_cpu: process.cpuUsage() };
}
async function upstream(latency) {
  const socket = dgram.createSocket('udp4'); let requests = 0; let error;
  const timers = new Set();
  socket.on('error', reason => { error = reason; });
  socket.on('message', (packet, peer) => {
    try {
      requests++; assert(timers.size < 8192, 'upstream fixture capacity');
      const response = aResponse(packet);
      if (!latency) socket.send(response, peer.port, peer.address);
      else {
        const timer = setTimeout(() => { timers.delete(timer); socket.send(response, peer.port, peer.address); }, latency);
        timers.add(timer);
      }
    } catch (reason) { error = reason; }
  });
  await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
  return { port: socket.address().port, count: () => requests, check: () => { if (error) throw error; },
    close: async () => { for (const timer of timers) clearTimeout(timer); await new Promise(resolve => socket.close(resolve)); } };
}
async function server(binary, directory, upstreamPort, kind, workers) {
  const config = `listen="127.0.0.1:0"
admin_listen="127.0.0.1:0"
query_timeout_ms=1000
tcp_io_timeout_ms=1000
shutdown_grace_ms=1000
max_inflight=4096
max_tcp_connections=64
[upstreams]
servers=["udp://127.0.0.1:${upstreamPort}"]
[cache]
enabled=${kind !== 'shared-miss'}
max_entries=200000
max_bytes=134217728
[cache.persistence]
enabled=false
[coalescing]
enabled=true
max_groups=4096
max_waiters=256
[filter_subscriptions]
enabled=false
`;
  const configFile = path.join(directory, 'config.toml');
  await writeFile(configFile, config, { mode: 0o600, flag: 'wx' });
  const log = createWriteStream(path.join(directory, 'server.log'), { mode: 0o600, flags: 'wx' });
  const child = spawn(binary, ['--config', configFile, '--data-dir', path.join(directory, 'data')],
    { env: { ...process.env, TOKIO_WORKER_THREADS: String(workers) }, stdio: ['ignore', 'pipe', 'pipe'] });
  let text = ''; child.stderr.on('data', b => { text = (text + b).slice(-65536); });
  child.stderr.pipe(log); child.stdout.resume();
  const exited = new Promise((resolve, reject) => { child.once('error', reject); child.once('exit', (code, signal) => resolve({ code, signal })); });
  const stop = async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    let outcome = await Promise.race([exited, delay(8000, null, { ref: false })]);
    if (!outcome) { child.kill('SIGKILL'); outcome = await exited; }
    await finished(log); assert.equal(outcome.code, 0, `server shutdown: ${JSON.stringify(outcome)}`);
  };
  try {
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      const dns = text.match(/PariNS listening on 127\.0\.0\.1:(\d+) \(UDP\/TCP\)/);
      const admin = text.match(/PariNS listening on 127\.0\.0\.1:(\d+) \(local metrics\)/);
      if (dns && admin) { await probe(Number(dns[1]), 'startup.bench.test', false); return { pid: child.pid, port: Number(dns[1]), admin: Number(admin[1]), stop }; }
      assert(child.exitCode === null && child.signalCode === null, `server startup failed: ${text}`);
      await delay(20);
    }
    throw new Error('server startup deadline');
  } catch (error) { await stop().catch(() => {}); throw error; }
}
async function trial(config, round, kind, rate, profile = false) {
  const name = `${profile ? 'profile' : `round-${round}`}-${kind}-${rate}`;
  const directory = path.join(config.output, name); await mkdir(directory, { mode: 0o700 });
  const peer = await upstream(kind === 'many-active' ? 200 : ['cold-unique', 'shared-miss'].includes(kind) ? 20 : 0);
  let service;
  try {
    if (kind !== 'direct') service = await server(config.binary, directory, peer.port, kind, config.workers);
    const port = service?.port ?? peer.port;
    const readMetrics = service ? () => metrics(service.admin) : async () => ({ upstream_success: peer.count() });
    let beforeUpstream;
    const result = await runWireLoad({ name, port, duration: config.seconds * 1000,
      slo: { rate, client_deadline_ms: 1000, refresh_at_ms: 1000 },
      expectCacheHit: kind !== 'direct' && kind !== 'shared-miss', resourceIntervalMs: 1000,
      warmup: async () => { await probe(port, 'warm.bench.test', false); beforeUpstream = peer.count(); },
      readMetrics, observeResources: () => resources(service?.pid ?? process.pid),
      itemFor: i => ({ name: kind === 'warm-cache' ? 'warm.bench.test' : kind === 'shared-miss'
        ? 'shared.bench.test' : `q-${i}.bench.test`, group: kind, blocked: false }),
      ...(profile ? { actionKey: 'profile', onAction: async () => {
        const file = path.join(directory, 'sample.txt');
        await exec('/usr/bin/sample', [String(service.pid), '3', '1', '-mayDie', '-file', file], { timeout: 10000, maxBuffer: 65536 });
        return { tool: 'macOS sample', seconds: 3, interval_ms: 1, file };
      } } : {}) }, async (row, { before, after, samplingError, actionError }) => {
        row.kind = kind; row.round = round; row.profiled = profile;
        row.workers = config.workers; row.host_load_after = os.loadavg();
        row.upstream_queries = peer.count() - beforeUpstream;
        row.cpu_seconds = after.cpu_seconds - before.cpu_seconds;
        row.cpu_scope = service ? 'server process ps CPU time, centisecond resolution' : 'driver and direct upstream process';
        row.cpu_average_cores = row.cpu_seconds / (row.actual_elapsed_ms / 1000);
        row.driver_cpu_average_cores = (after.driver_cpu.user + after.driver_cpu.system - before.driver_cpu.user - before.driver_cpu.system) / (row.actual_elapsed_ms * 1000);
        row.rss_sample_peak_bytes = Math.max(...row.resource_samples.map(s => s.rss_bytes));
        row.resource_scope = 'process RSS sampled each second; no cgroup, OOM or lifetime-peak measurement';
        row.failures = [];
        if (samplingError) row.failures.push('resource_sampling');
        if (actionError) row.failures.push('profile_capture');
        if (samplingError || actionError) row.observation_error = String((samplingError ?? actionError).message).slice(0, 4096);
        if (row.correct !== row.planned || row.timeouts || row.invalid || row.unexpected || row.send_errors || row.never_sent) row.failures.push('correctness_or_offered_load');
        if (kind === 'warm-cache' && (row.metric_deltas.cache_hits !== row.planned || row.upstream_queries !== 0)) row.failures.push('warm_cache_contract');
        if (['cold-unique', 'many-active'].includes(kind) && row.upstream_queries !== row.planned) row.failures.push('unique_upstream_contract');
        if (kind === 'shared-miss' && (row.metric_deltas.cache_hits !== 0 || row.upstream_queries >= row.planned)) row.failures.push('coalescing_contract');
        row.result = row.failures.length ? 'failed' : 'passed';
        await writeFile(path.join(directory, 'result.json'), JSON.stringify(row, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
        console.log(JSON.stringify({ name, result: row.result, correct_qps: row.correct_qps,
          p99_ms: row.scheduled_latency_ms.p99, driver_p99_ms: row.driver_lag_ms.p99, failures: row.failures }));
        return row;
      });
    peer.check(); return result;
  } finally { try { await service?.stop(); } finally { await peer.close(); } }
}
export async function main(args) {
  const config = options(args);
  await mkdir(path.dirname(config.output), { recursive: true, mode: 0o700 });
  await mkdir(config.output, { mode: 0o700 });
  const info = JSON.parse((await exec(config.binary, ['--build-info=json'], { timeout: 5000 })).stdout);
  const provenance = { started_at: new Date().toISOString(), config, build: info,
    binary_sha256: createHash('sha256').update(await readFile(config.binary)).digest('hex'),
    harness_sha256: Object.fromEntries(await Promise.all(['scripts/bench-dns.mjs', 'scripts/lib/managed-wire.mjs', 'scripts/lib/wire-bench.mjs']
      .map(async file => [file, createHash('sha256').update(await readFile(path.join(root, file))).digest('hex')]))),
    source_commit: (await exec('git', ['rev-parse', 'HEAD'], { cwd: root })).stdout.trim(),
    tracked_diff: (await exec('git', ['diff', 'HEAD'], { cwd: root })).stdout,
    node: process.version, platform: process.platform, arch: process.arch, release: os.release(),
    cpu: os.cpus()[0].model, logical_cpus: os.cpus().length, memory_bytes: os.totalmem(), host_load_before: os.loadavg(),
    topology: 'separate production server process; driver and synthetic upstream share one Node process on this host',
    transport: 'UDP loopback; fixed offered load; every reply checked; no public DNS or host configuration changes' };
  await writeFile(path.join(config.output, 'provenance.json'), JSON.stringify(provenance, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  const rows = [];
  try {
    for (let round = 0; round < config.repeats; round++) {
      const matrix = config.kinds.flatMap(kind => config.rates.map(rate => [kind, rate]));
      const shift = round % matrix.length;
      for (const [kind, rate] of [...matrix.slice(shift), ...matrix.slice(0, shift)]) rows.push(await trial(config, round, kind, rate));
    }
    if (config.profile) rows.push(await trial(config, 0, 'many-active', Math.max(...config.rates), true));
  } finally {
    const summary = config.kinds.flatMap(kind => config.rates.map(rate => {
      const trials = rows.filter(r => r.kind === kind && r.configured_qps === rate && !r.profiled);
      const failures = trials.filter(r => r.result !== 'passed').length;
      return { kind, offered_qps: rate, rounds: trials.length, failures,
        result: trials.length === 0 ? 'NOT_RUN' : failures ? 'failed' : trials.length < config.repeats ? 'incomplete' : 'passed',
        correct_qps: distribution(trials.map(r => r.correct_qps)),
        scheduled_p99_ms: distribution(trials.map(r => r.scheduled_latency_ms.p99)),
        driver_p99_ms: distribution(trials.map(r => r.driver_lag_ms.p99)),
        cpu_scope: kind === 'direct' ? 'driver and direct upstream process' : 'server process',
        cpu_average_cores: distribution(trials.map(r => r.cpu_average_cores)) };
    }));
    await writeFile(path.join(config.output, 'summary.json'), JSON.stringify(summary, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  }
  assert(rows.every(r => r.result === 'passed'), 'baseline contains failed trials; retained all results');
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch(error => { console.error(error); process.exitCode = 1; });
}

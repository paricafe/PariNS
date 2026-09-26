// Isolated wire benchmark, never host DNS/services/trust or public traffic.
// Default: logs off/on × hot/miss × 3 repeats; 20,000 UDP requests, concurrency 32.
// --ecs-padding: automatic outgoing ECS, upstream returns Padding but no ECS.
// Both versions disable coalescing in this mode. This compares cache BEHAVIOR,
// not equivalent hot paths. For v0.1.3 add --expect-uncached-hot.
// --persistent passes --data-dir; --smoke uses 128 queries × 4 cases, not perf data.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import dgram from 'node:dgram';
import { query, parse, bounded, resource, metrics } from './lib/wire-bench.mjs';
import { mkdtemp, writeFile, stat } from 'node:fs/promises';
import { tmpdir, platform } from 'node:os';
import path from 'node:path';
import { performance } from 'node:perf_hooks';

const flags = new Set(process.argv.slice(3));
for (const flag of flags) assert(['--persistent', '--ecs-padding', '--expect-uncached-hot', '--smoke'].includes(flag), `unknown flag ${flag}`);
const binary = path.resolve(process.argv[2] ?? 'target/release/parins');
const persistent = flags.has('--persistent'), ecsPadding = flags.has('--ecs-padding');
const uncachedHot = flags.has('--expect-uncached-hot'), smoke = flags.has('--smoke');
assert(!uncachedHot || ecsPadding, '--expect-uncached-hot requires --ecs-padding');
const count = Number(process.env.PARINS_BENCH_QUERIES ?? (smoke ? 128 : 20000));
assert(Number.isInteger(count) && count > 0 && count <= 65000, 'PARINS_BENCH_QUERIES must be 1..65000 (unique IDs)');
const concurrency = 32, repeats = smoke ? 1 : 3;
const root = await mkdtemp(path.join(tmpdir(), 'parins-wire-bench-'));
const ecs = Buffer.from([0, 1, 24, 0, 127, 0, 0]);
const upstream = dgram.createSocket('udp4');
let upstreamCount = 0, upstreamEcsCount = 0;
const fixtureErrors = [];
upstream.on('message', (wire, peer) => {
  upstreamCount++;
  try {
    const parsed = parse(wire), received = parsed.options.filter(option => option.code === 8);
    if (ecsPadding) {
      assert(received.length === 1 && received[0].data.equals(ecs), 'fixture must actually receive ECS 127.0.0.0/24'); upstreamEcsCount++;
    } else assert.equal(received.length, 0, 'original workload must not send ECS');
    const header = Buffer.from(wire.subarray(0, 12));
    header.writeUInt16BE(0x8180, 2); header.writeUInt16BE(1, 6); header.writeUInt16BE(0, 8); header.writeUInt16BE(ecsPadding ? 1 : 0, 10);
    const answer = Buffer.from([0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 1]);
    // Answer must precede OPT; never append it behind the copied query's OPT.
    const padding = ecsPadding ? Buffer.concat([Buffer.from([0, 0, 41, 4, 208, 0, 0, 0, 0, 0, 36, 0, 12, 0, 32]), Buffer.alloc(32, 0x5a)]) : Buffer.alloc(0);
    upstream.send(Buffer.concat([header, wire.subarray(12, parsed.questionEnd), answer, padding]), peer.port, peer.address);
  } catch (error) { fixtureErrors.push(error.message); }
});
await new Promise(resolve => upstream.bind(0, '127.0.0.1', resolve));
async function storage(dir, logging, completed, cleanExit) {
  const result = { database_bytes: null, wal_bytes: null, shm_bytes: null, snapshot_bytes: null, log_retained: null, log_cleanup: null, log_dropped_inferred_total: null, log_drop_reasons: null, storage_observation_error: null };
  if (!persistent) return { ...result, log_accounting_note: 'legacy in-memory history has no persisted coverage/drop counters' };
  const bytes = async name => { try { return (await stat(path.join(dir, 'data', name))).size; } catch (error) { if (error.code === 'ENOENT') return 0; throw error; } };
  result.database_bytes = await bytes('observability.sqlite3'); result.wal_bytes = await bytes('observability.sqlite3-wal');
  result.shm_bytes = await bytes('observability.sqlite3-shm'); result.snapshot_bytes = await bytes('dns-cache-clean.snapshot');
  try {
    const rows = JSON.parse(execFileSync('sqlite3', ['-readonly', '-json', path.join(dir, 'data', 'observability.sqlite3'), 'SELECT key,value FROM metadata;'], { encoding: 'utf8', timeout: 5000 }));
    const metadata = Object.fromEntries(rows.map(row => [row.key, row.value]));
    result.log_retained = metadata.log_count ?? null;
    const reasons = ['age', 'entry_limit', 'byte_limit', 'database_limit', 'manual'];
    assert(reasons.every(reason => Number.isInteger(metadata[`cleanup_${reason}`])), 'cleanup metadata unavailable');
    result.log_cleanup = Object.fromEntries(reasons.map(reason => [reason, metadata[`cleanup_${reason}`]]));
    if (logging && cleanExit && completed !== null && result.log_retained !== null) {
      const missing = completed - result.log_retained - Object.values(result.log_cleanup).reduce((sum, value) => sum + value, 0);
      assert(missing >= 0, 'log conservation count is inconsistent'); result.log_dropped_inferred_total = missing;
    }
  } catch (error) { result.storage_observation_error = error.message; }
  return { ...result, log_accounting_note: 'inferred only with logging enabled, clean exit and validated responses: completed including warmup - retained - recorded cleanup; no external queries/clears/config changes; drop reasons unknown' };
}
try {
  for (const logging of [false, true]) for (const hit of [true, false]) for (let repeat = 1; repeat <= repeats; repeat++) {
    const dir = await mkdtemp(path.join(root, 'case-'));
    const config = `listen='127.0.0.1:0'\nadmin_listen='127.0.0.1:0'\nquery_timeout_ms=1000\ntcp_io_timeout_ms=1000\nshutdown_grace_ms=1000\nmax_inflight=128\nmax_tcp_connections=32\n[upstreams]\nservers=['udp://127.0.0.1:${upstream.address().port}']\n[query_log]\nenabled=${logging}\nmax_entries=1000\nretention_secs=86400\n${ecsPadding ? '[ecs]\nenabled=true\nipv4_prefix=24\nipv6_prefix=56\n[coalescing]\nenabled=false\n' : ''}`;
    await writeFile(path.join(dir, 'config.toml'), config, { mode: 0o600 });
    const args = ['--config', path.join(dir, 'config.toml')];
    if (persistent) args.push('--data-dir', path.join(dir, 'data'));
    const started = performance.now();
    // Private cwd also isolates older binaries lacking --data-dir.
    const child = spawn('/usr/bin/time', [platform() === 'darwin' ? '-l' : '-v', binary, ...args], { cwd: dir, detached: true, stdio: ['ignore', 'ignore', 'pipe'] });
    let stderr = '', pid, stopped = false;
    child.stderr.on('data', chunk => { stderr += chunk; });
    const exited = new Promise((resolve, reject) => { child.on('close', code => resolve(code)); child.on('error', reject); });
    const socket = dgram.createSocket('udp4');
    try {
      const port = await bounded(new Promise((resolve, reject) => {
        const inspect = () => { const match = stderr.match(/PariNS listening on 127\.0\.0\.1:(\d+) \(UDP\/TCP\)/); if (match) { child.stderr.off('data', inspect); resolve(Number(match[1])); } };
        child.stderr.on('data', inspect); inspect();
        exited.then(code => reject(new Error(`startup exited ${code}: ${stderr}`)), reject);
      }), 10000, 'startup timeout');
      const startupMs = performance.now() - started;
      pid = Number(execFileSync('pgrep', ['-P', String(child.pid)], { encoding: 'utf8' }).trim().split('\n')[0]);
      assert(Number.isSafeInteger(pid) && pid > 1, 'missing owned server PID');
      await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
      const pending = new Map();
      let dnsTimeouts = 0, dnsInvalid = 0, sendErrors = 0, attempted = 0, halt = false;
      socket.on('message', (wire, peer) => {
        if (wire.length < 2 || peer.port !== port || peer.address !== '127.0.0.1') return;
        const id = wire.readUInt16BE(0), entry = pending.get(id); if (!entry) return;
        pending.delete(id); clearTimeout(entry.timeout);
        try {
          const parsed = parse(wire);
          assert((wire.readUInt16BE(2) & 0x820f) === 0x8000, 'DNS error/truncated/non-response');
          assert(wire.subarray(12, parsed.questionEnd).equals(entry.question), 'wrong question');
          assert.equal(parsed.answers.length, 1);
          assert(parsed.answers[0].type === 1 && parsed.answers[0].klass === 1 && parsed.answers[0].data.equals(Buffer.from([192, 0, 2, 1])), 'wrong A answer');
          entry.resolve((performance.now() - entry.started) * 1000);
        } catch { dnsInvalid++; halt = true; entry.resolve(null); }
      });
      const request = (id, name) => new Promise(resolve => {
        const wire = query(id, name);
        const timeout = setTimeout(() => { pending.delete(id); dnsTimeouts++; halt = true; resolve(null); }, 5000);
        pending.set(id, { resolve, timeout, started: performance.now(), question: wire.subarray(12) });
        socket.send(wire, port, '127.0.0.1', error => { if (error && pending.delete(id)) { clearTimeout(timeout); sendErrors++; halt = true; resolve(null); } });
      });
      const warmupBefore = upstreamCount, warmupEcsBefore = upstreamEcsCount;
      if (hit) assert(await request(65000, 'warm.bench.test') !== null, 'warmup failed');
      const warmupUpstream = upstreamCount - warmupBefore, warmupEcsVerified = upstreamEcsCount - warmupEcsBefore;
      const upstreamBefore = upstreamCount, ecsBefore = upstreamEcsCount, fixtureBefore = fixtureErrors.length, samples = [];
      let next = 0;
      const begin = performance.now();
      await Promise.all(Array.from({ length: concurrency }, async () => {
        while (next < count && !halt) { const id = next++; attempted++; const latency = await request(id, hit ? 'warm.bench.test' : `q${id}.bench.test`); if (latency !== null) samples.push(latency); }
      }));
      const elapsed = performance.now() - begin;
      samples.sort((a, b) => a - b);
      const adminPort = Number(stderr.match(/PariNS local metrics on 127\.0\.0\.1:(\d+)/)?.[1]);
      const counters = adminPort ? await metrics(adminPort) : null;
      const stopping = performance.now();
      process.kill(pid, 'SIGTERM');
      const code = await bounded(exited, 15000, 'server shutdown timeout');
      const stopMs = performance.now() - stopping; stopped = true; pid = undefined;
      const actualUpstream = upstreamCount - upstreamBefore, expectedUpstream = hit && !uncachedHot ? 0 : count;
      const valid = code === 0 && attempted === count && samples.length === count && actualUpstream === expectedUpstream && fixtureErrors.length === fixtureBefore && counters?.udp_dropped === 0 && counters?.dropped === 0;
      const observation = await storage(dir, logging, valid ? count + Number(hit) : null, code === 0);
      await writeFile(path.join(dir, 'server.stderr'), stderr, { mode: 0o600 });
      console.log(JSON.stringify({ binary, workload: ecsPadding ? 'missing_ecs_padding' : 'original', smoke, logging, hit, repeat, count, attempted, completed: samples.length, concurrency,
        comparison: ecsPadding ? 'cache behavior change, not equivalent hot paths; coalescing disabled on BOTH versions' : 'same original hot/miss workload; coalescing enabled',
        outgoing_ecs: ecsPadding ? 'automatic socket-peer ECS 127.0.0.0/24 verified by fixture' : null,
        expected_hot: uncachedHot ? 'uncached' : 'cached', upstream: actualUpstream, expected_upstream: expectedUpstream, upstream_ecs_verified: upstreamEcsCount - ecsBefore, warmup_upstream: warmupUpstream, warmup_ecs_verified: warmupEcsVerified,
        correctness_passed: valid, dns_timeouts: dnsTimeouts, dns_invalid_responses: dnsInvalid, dns_send_errors: sendErrors,
        server_udp_dropped: counters?.udp_dropped ?? null, server_dropped: counters?.dropped ?? null,
        qps: samples.length / elapsed * 1000, p95_us: samples[Math.ceil(samples.length * .95) - 1] ?? null, p99_us: samples[Math.ceil(samples.length * .99) - 1] ?? null,
        startup_ms: startupMs, stop_ms: stopMs, ...resource(stderr), ...observation,
        write_bytes: null, write_bytes_note: 'exact disk write bytes unmeasured; OS output counts and final file sizes are separate observations', fixture: dir }));
      assert(valid, `correctness failed: ${dir}/server.stderr; fixture errors: ${fixtureErrors.slice(fixtureBefore)}`);
      assert(!persistent || !observation.storage_observation_error, `storage observation failed: ${observation.storage_observation_error}`);
    } finally {
      try { socket.close(); } catch { /* Startup may fail before bind. */ }
      if (!stopped) {
        // Only the new process group owned by this case, never host services.
        try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
        await bounded(exited.catch(() => null), 2000, 'owned process group did not exit');
      }
    }
  }
} finally { upstream.close(); }

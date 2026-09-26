// FS1e: test-only Policy injection through the real Server/Resolver, never production.
// Linux isolation is launched only by test-filter-performance.sh --ephemeral-ci.
import assert from 'node:assert/strict';
import { spawn, spawnSync, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { cpus, totalmem, tmpdir, platform, release } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance, PerformanceObserver, monitorEventLoopDelay } from 'node:perf_hooks';
import { query, parse, bounded, resource, metrics, cpuList, linuxConstraints, verifyLinuxServer, linuxUsage } from './lib/wire-bench.mjs';

assert(process.argv.length >= 4 && process.argv.length <= 8, 'need libtest executable and fixed corpus');
const flags = process.argv.slice(4);
assert(flags.every(flag => ['--smoke', '--native-acceptance', '--diagnose', '--profile', '--driver-comparison', '--linux-isolated'].includes(flag)) && new Set(flags).size === flags.length, 'unknown or repeated benchmark mode');
assert(!(flags.includes('--native-acceptance') && flags.includes('--diagnose')), 'diagnostic is not acceptance');
const smoke = flags.includes('--smoke'), diagnose = flags.includes('--diagnose'), native = flags.includes('--native-acceptance') || diagnose;
const profile = flags.includes('--profile');
const driverComparison = flags.includes('--driver-comparison');
const isolated = flags.includes('--linux-isolated');
assert(!isolated || (platform() === 'linux' && flags.includes('--native-acceptance') && process.env.GITHUB_ACTIONS === 'true' && process.env.RUNNER_ENVIRONMENT === 'github-hosted' && process.env.RUNNER_OS === 'Linux'), '--linux-isolated requires native acceptance on a disposable hosted Linux runner');
const serviceCpus = isolated ? cpuList(process.env.PARINS_FS_SERVER_CPUS) : null;
const driverCpus = isolated ? cpuList(process.env.PARINS_FS_DRIVER_CPUS) : null;
if (isolated) {
  assert.equal(serviceCpus.length, 2, 'exactly two service CPUs required');
  assert(driverCpus.length >= 2 && serviceCpus.every(cpu => !driverCpus.includes(cpu)), 'at least two separate driver CPUs required');
  assert.deepEqual(cpuList((await linuxConstraints('self')).allowed_cpus), driverCpus, 'launch driver with the declared affinity');
}
assert(!profile || (diagnose && !smoke), '--profile requires --diagnose without --smoke');
assert(!driverComparison || (diagnose && !profile), '--driver-comparison requires --diagnose without --profile');
assert(!diagnose || platform() === 'darwin', '--diagnose ps sampler currently requires macOS');
const rounds = smoke || profile ? 1 : 3, warmupCount = native ? 256 : 0;
const binary = path.resolve(process.argv[2]), rules = path.resolve(process.argv[3]);
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const corpus = await readFile(rules);
assert.equal(sha(corpus), 'd68e37b2a861e6e8ef85568db4237bb3e18d1a9f2476323b3dba977fe850af09');
const domains = corpus.toString('utf8').trimEnd().split('\n').map(line => {
  assert(line.startsWith('.')); const name = line.slice(1);
  assert(name.length + 2 <= 253 && name.split('.').every(label => label.length <= 63)); return name;
});
assert.equal(domains.length, 201337);
const indicesFor = count => Array.from({ length: count }, (_, i) => Math.floor(i * (domains.length - 1) / (count - 1)));
const groupsFor = (count, mode) => { const indices = indicesFor(count); const groups = [
  { name: 'corpus_root', kind: 'query_blocked', names: indices.map(i => domains[i]) },
  { name: 'corpus_child', kind: 'query_blocked', names: indices.map(i => `x.${domains[i]}`) },
  { name: 'fresh_cache', kind: 'hit', names: Array(count).fill('warm.fs-wire.test') },
  { name: 'allow_exact', kind: 'hit', names: Array(count).fill('allow.blocked.fs-wire.test') },
  { name: 'allow_suffix', kind: 'hit', names: Array(count).fill('x.safe.blocked.fs-wire.test') },
  { name: 'upstream_miss', kind: 'miss', names: Array.from({ length: count }, (_, i) => `q${i}.miss.fs-wire.test`) },
  { name: 'cname_miss', kind: 'cname_miss', names: Array.from({ length: count }, (_, i) => `q${i}.aliases.fs-wire.test`) },
  { name: 'cname_cached', kind: 'cname_hit', names: Array(count).fill('alias.fs-wire.test') },
];
  if (!diagnose) return groups;
  if (profile) return [...groups.slice(5, 7), ...Array.from({ length: 4 }, (_, i) => ({ ...groups[7], name: `cname_cached_sample_${i + 1}` }))];
  return mode === 'paced_1000' ? groups.slice(5, 7) : [{ ...groups[7], name: 'cname_cached_clean' }, ...groups.slice(5)];
};
const modes = driverComparison ? ['root', 'worker'].map(driver => ({ name: `closed_8_${driver}`, driver, concurrency: 8, offered_qps: null, count: smoke ? 32 : 60000 })) : [
  { name: 'paced_1000', concurrency: 1, offered_qps: 1000, count: smoke ? 32 : 4096 },
  { name: native ? 'closed_8' : 'closed_32', concurrency: native ? 8 : 32, offered_qps: null, count: smoke ? 32 : native ? 60000 : 4096 },
].filter(mode => !profile || mode.name === 'closed_8');
const root = await mkdtemp(path.join(tmpdir(), 'parins-filter-wire-'));
const sources = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', ...execFileSync('git', ['ls-files', 'src'], { cwd: repo, encoding: 'utf8' }).trim().split('\n'), 'scripts/bench-filter-wire.mjs', 'scripts/lib/wire-bench.mjs', 'scripts/test-filter-performance.sh'];
const sourceHashes = Object.fromEntries(await Promise.all(sources.map(async file => [file, sha(await readFile(path.join(repo, file)))])));
const manifest = {
  command: process.argv, smoke, native_acceptance: native && !diagnose, diagnostic: diagnose, sampling_profile: profile, driver_comparison: driverComparison, binary, binary_sha256: sha(await readFile(binary)), source_sha256: sourceHashes,
  platform: platform(), os_release: release(), cpu_model: cpus()[0]?.model, logical_cpus: cpus().length, host_memory_bytes: totalmem(),
  rustc: execFileSync('rustc', ['--version'], { encoding: 'utf8' }).trim(), build_profile: 'cargo test --locked --release --lib --no-run; default release opt-level=3; cfg(test)',
  worker_threads: 2, os_cpu_limit: isolated ? { requested_service_cpus: serviceCpus, requested_driver_cpus: driverCpus, memory_max_bytes: 4294967296, swap_max_bytes: 0, mechanism: 'systemd transient cgroup v2 plus per-thread affinity; actual constraints verified before each measured process' } : null,
  driver_constraints: isolated ? await linuxConstraints('self') : null,
  scope: isolated ? 'Linux resource-isolated static trie/radix comparison; host IRQ/other-process interference is not excluded; not subscription lifecycle or capacity acceptance' : 'Darwin exploratory when on Darwin; not Linux isolated 2-core capacity or subscription lifecycle acceptance',
  rules_sha256: sha(corpus), rules_bytes: corpus.length, input_rules: domains.length + 6,
  first_line: 1, last_line: domains.length,
  modes: modes.map(mode => ({ ...mode, sample_indices_zero_based: indicesFor(mode.count), sampled_lines: mode.count, sample_fraction: mode.count / domains.length,
    groups: groupsFor(mode.count, mode.name).map(group => ({ name: group.name, kind: group.kind, request_sequence_sha256: sha(Buffer.concat(group.names.map((name, id) => query(id, name)))) })) })),
  rounds, warmup_requests_per_group: native ? warmupCount : 'one for hit groups, zero otherwise', server_processes: modes.length * rounds * (driverComparison ? 1 : 2),
  timeout_ms: 5000, upstream: 'loopback UDP, immediate A 192.0.2.1 TTL60 or reachable CNAME target.fs-wire.test + A; automatic ECS 127.0.0.0/24 echoed scope0',
  resources: 'time whole server lifecycle includes full canonical parse/build/startup/shutdown; client CPU per measured group includes local mock and validation; not active index bytes',
  cache: '16384 entries,64MiB,4 shards; prefetch/stale/persistence/coalescing/logging disabled; miss warmup uses disjoint names',
  timing: 'service latency starts before send; scheduled latency starts at intended send time in paced mode; validation included in both; paced concurrency1 can accumulate lag (not an independent open-loop generator)',
  diagnostic_scope: diagnose ? 'CNAME clean/after-miss ordering and paced miss decomposition only; extra instrumentation, not directly comparable performance or acceptance. Server timer is resolver internal, not wire; ps CPU centisecond resolution. Fixed 1024-completion windows; GC/event-loop observations cannot alone establish cause.' : null,
  profile_scope: profile ? 'Qualitative macOS sample: two servers, one 1-second stack capture during four cached batches after 120k miss prelude. Sampling perturbs timing; not a performance comparison.' : null,
  driver_scope: driverComparison ? 'Radix only, root block_on driver versus worker task; AB/BA/AB. Same binary/two workers/workload, no sampling or product change. Causal scheduling probe, not an index adoption gate.' : null,
};
await writeFile(path.join(root, 'manifest.json'), JSON.stringify(manifest, null, 2), { mode: 0o600 });
console.log(JSON.stringify({ event: 'manifest_saved_before_measurement', root, binary_sha256: manifest.binary_sha256, smoke, native, modes, rounds }));

const ecs = Buffer.from([0, 1, 24, 0, 127, 0, 0]), aData = Buffer.from([192, 0, 2, 1]);
const upstream = dgram.createSocket('udp4');
let upstreamCount = 0, ecsCount = 0;
const fixtureErrors = [], records = [];
const encodeName = name => query(0, name).subarray(12, -4);
const targetName = encodeName('target.fs-wire.test');
function rr(owner, type, data) {
  const fields = Buffer.alloc(10); fields.writeUInt16BE(type, 0); fields.writeUInt16BE(1, 2); fields.writeUInt32BE(60, 4); fields.writeUInt16BE(data.length, 8);
  return Buffer.concat([owner, fields, data]);
}
upstream.on('message', (wire, peer) => {
  upstreamCount++;
  try {
    const parsed = parse(wire), options = parsed.options.filter(option => option.code === 8);
    assert.equal(options.length, 1); assert(options[0].data.equals(ecs), 'outgoing peer ECS mismatch'); ecsCount++;
    // Query names are controlled uncompressed fixture names, not a product DNS parser.
    const labels = []; for (let at = 12; wire[at];) { const length = wire[at++]; labels.push(wire.subarray(at, at + length).toString('ascii')); at += length; }
    const name = labels.join('.'), cname = name === 'alias.fs-wire.test' || name.endsWith('.aliases.fs-wire.test');
    const header = Buffer.from(wire.subarray(0, 12)); header.writeUInt16BE(0x8180, 2); header.writeUInt16BE(cname ? 2 : 1, 6); header.writeUInt16BE(0, 8); header.writeUInt16BE(1, 10);
    const answer = cname ? Buffer.concat([rr(Buffer.from([0xc0, 12]), 5, targetName), rr(targetName, 1, aData)]) : rr(Buffer.from([0xc0, 12]), 1, aData);
    const opt = Buffer.concat([Buffer.from([0, 0, 41, 4, 208, 0, 0, 0, 0, 0, 11, 0, 8, 0, 7]), ecs]);
    upstream.send(Buffer.concat([header, wire.subarray(12, parsed.questionEnd), answer, opt]), peer.port, peer.address, error => { if (error) fixtureErrors.push(error.message); });
  } catch (error) { fixtureErrors.push(error.message); }
});
await new Promise(resolve => upstream.bind(0, '127.0.0.1', resolve));
const percentile = (samples, q) => samples[Math.ceil(samples.length * q) - 1] ?? null;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const gcEvents = [];
const gcObserver = diagnose ? new PerformanceObserver(list => { for (const entry of list.getEntries()) gcEvents.push({ start: entry.startTime, duration: entry.duration }); }) : null;
gcObserver?.observe({ entryTypes: ['gc'] });
function serverCpu(pid) {
  const text = execFileSync('ps', ['-p', String(pid), '-o', 'time='], { encoding: 'utf8' }).trim();
  assert(/^\d+:\d\d\.\d\d$/.test(text), `unrecognized ps CPU time: ${text}`);
  const [minutes, seconds] = text.split(':').map(Number); return minutes * 60 + seconds;
}
function distribution(samples) {
  const sorted = [...samples].sort((a, b) => a - b);
  return { count: sorted.length, mean: sorted.length ? sorted.reduce((a, b) => a + b, 0) / sorted.length : null, p50: percentile(sorted, .5), p95: percentile(sorted, .95), p99: percentile(sorted, .99), max: sorted.at(-1) ?? null };
}
let semanticDigest;
async function run(engine, round, mode) {
  const count = mode.count, groups = groupsFor(count, mode.name);
  const dir = await mkdtemp(path.join(root, `${mode.name}-${round}-${engine}-`));
  const config = `listen='127.0.0.1:0'\nadmin_listen='127.0.0.1:0'\nquery_timeout_ms=1000\ntcp_io_timeout_ms=1000\nshutdown_grace_ms=1000\nmax_inflight=128\nmax_tcp_connections=32\n[upstreams]\nservers=['udp://127.0.0.1:${upstream.address().port}']\n[ecs]\nenabled=true\nipv4_prefix=24\n[cache]\nmax_entries=16384\nmax_bytes=67108864\nshards=4\n[cache.prefetch]\nenabled=false\n[cache.stale]\nenabled=false\n[cache.persistence]\nenabled=false\n[coalescing]\nenabled=false\n[query_log]\nenabled=false\n`;
  await writeFile(path.join(dir, 'config.toml'), config, { mode: 0o600 });
  const started = performance.now();
  const fixtureEnv = { PARINS_FS_WIRE_CONFIG: path.join(dir, 'config.toml'), PARINS_FS_WIRE_RULES: rules, PARINS_FS_WIRE_ENGINE: engine, PARINS_FS_WIRE_DRIVER: mode.driver ?? 'root', LC_ALL: 'C' };
  const fixtureArgs = [platform() === 'darwin' ? '-l' : '-v', binary, '--exact', 'policy::wire::serve', '--ignored', '--nocapture', '--test-threads=1'];
  const unit = isolated ? `parins-fs-wire-${process.pid}-${path.basename(dir)}.service` : null;
  const child = spawn(isolated ? 'sudo' : '/usr/bin/time', isolated ? ['-n', 'systemd-run', '--quiet', '--wait', '--pipe', '--collect', '--service-type=exec', `--unit=${unit}`, `--uid=${process.getuid()}`, `--gid=${process.getgid()}`, `--working-directory=${dir}`, `--property=AllowedCPUs=${serviceCpus.join(',')}`, `--property=CPUAffinity=${serviceCpus.join(' ')}`, '--property=MemoryMax=4294967296', '--property=MemorySwapMax=0', '--property=KillMode=control-group', '--property=TimeoutStopSec=10s', '--property=RuntimeMaxSec=300s', ...Object.entries(fixtureEnv).map(([key, value]) => `--setenv=${key}=${value}`), '--', '/usr/bin/time', ...fixtureArgs] : fixtureArgs, {
    cwd: dir, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, ...fixtureEnv },
  });
  let stdout = '', stderr = '', stopped = false, sampling;
  const cancelOwnedUnit = () => {
    if (isolated) spawnSync('sudo', ['-n', 'systemctl', 'stop', unit], { encoding: 'utf8', timeout: 15000 });
  };
  if (isolated) { process.on('SIGTERM', cancelOwnedUnit); process.on('SIGINT', cancelOwnedUnit); }
  child.stdout.on('data', chunk => { stdout += chunk; }); child.stderr.on('data', chunk => { stderr += chunk; });
  const exited = new Promise((resolve, reject) => { child.on('close', resolve); child.on('error', reject); });
  const socket = dgram.createSocket('udp4');
  try {
    const ready = await bounded(new Promise((resolve, reject) => {
      const inspect = () => { const match = stdout.match(/PARINS_FS_WIRE_READY (\{[^\n]+\})/); if (match) { child.stdout.off('data', inspect); resolve(JSON.parse(match[1])); } };
      child.stdout.on('data', inspect); inspect(); exited.then(code => reject(new Error(`startup exited ${code}: ${stderr} ${stdout}`)), reject);
    }), 30000, 'startup timeout');
    const startupMs = performance.now() - started;
    assert.equal(ready.engine, engine); assert.equal(ready.input_rules, manifest.input_rules);
    assert.equal(ready.driver, mode.driver ?? 'root');
    semanticDigest ??= ready.semantic_digest; assert.equal(ready.semantic_digest, semanticDigest);
    const port = Number(ready.dns.split(':').at(-1)), adminPort = Number(ready.metrics.split(':').at(-1));
    const pid = ready.pid;
    assert(Number.isSafeInteger(pid) && pid > 1, 'missing owned fixture PID');
    const constraints = isolated ? await verifyLinuxServer(pid, unit, serviceCpus, driverCpus) : null;
    if (isolated) await writeFile(path.join(dir, 'constraints-before.json'), JSON.stringify(constraints, null, 2), { mode: 0o600 });
    await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
    const pending = new Map(); let active;
    socket.on('message', (wire, peer) => {
      const arrived = diagnose ? performance.now() : 0;
      if (!active) return;
      const id = wire.length >= 2 ? wire.readUInt16BE(0) : -1, entry = pending.get(id);
      if (!entry || peer.port !== port || peer.address !== '127.0.0.1') { active.unexpected++; return; }
      pending.delete(id); clearTimeout(entry.timeout); active.received++;
      try {
        const parsed = parse(wire);
        assert.equal(wire.readUInt16BE(2), 0x8180, 'QR/RD/RA/opcode/rcode/TC/AD/CD flags');
        assert(wire.subarray(12, parsed.questionEnd).equals(entry.question), 'question mismatch');
        assert.equal(wire.readUInt16BE(8), 0, 'unexpected authority');
        assert.equal(wire.readUInt16BE(10), 0, 'non-EDNS client must not receive automatic ECS/OPT');
        assert.equal(parsed.options.length, 0);
        if (entry.blocked) { assert.equal(parsed.answers.length, 0); active.expected_nodata++; }
        else {
          assert.equal(parsed.answers.length, 1); const a = parsed.answers[0];
          assert(a.type === 1 && a.klass === 1 && a.data.equals(aData), 'wrong A');
          assert.deepEqual(a.owner, entry.name.split('.').map(label => Buffer.from(label)), 'wrong answer owner');
          assert(a.ttl > 0 && a.ttl <= 60, 'wrong fresh answer TTL'); active.successful_a++;
        }
        const completed = performance.now();
        active.latencies.push((completed - entry.started) * 1000);
        if (entry.due !== undefined) active.scheduledLatencies.push((completed - entry.due) * 1000);
        if (diagnose) {
          active.receiveLatencies.push((arrived - entry.started) * 1000);
          active.validationLatencies.push((completed - arrived) * 1000);
          if (active.windowStart !== undefined && active.received % 1024 === 0) {
            const cpu = process.cpuUsage(active.windowCpu);
            active.windows.push({ completed: active.received, elapsed_ms: completed - active.windowStart, cpu_ms: (cpu.user + cpu.system) / 1000 });
            active.windowStart = completed; active.windowCpu = process.cpuUsage();
          }
        }
        entry.resolve();
      } catch (error) { active.invalid++; active.errors.push(error.message); entry.resolve(); }
    });
    const request = (id, name, blocked, due) => new Promise(resolve => {
      const wire = query(id, name); active.attempted++;
      const timeout = setTimeout(() => { pending.delete(id); active.timeouts++; resolve(); }, 5000);
      pending.set(id, { resolve, timeout, started: performance.now(), due, question: wire.subarray(12), name, blocked });
      socket.send(wire, port, '127.0.0.1', error => {
        if (!error) active.sent++;
        else if (pending.delete(id)) { clearTimeout(timeout); active.send_errors++; active.errors.push(error.message); resolve(); }
      });
    });
    const counters = () => ({ attempted: 0, sent: 0, received: 0, successful_a: 0, expected_nodata: 0, invalid: 0, timeouts: 0, send_errors: 0, unexpected: 0, errors: [], latencies: [], scheduledLatencies: [], receiveLatencies: [], validationLatencies: [], windows: [] });
    for (const group of groups) {
      const hit = group.kind.endsWith('hit'), blocked = group.kind === 'query_blocked' || group.kind.startsWith('cname');
      const warmupBefore = upstreamCount, warmupMetrics = native ? await metrics(adminPort) : null, warmupErrors = fixtureErrors.length;
      active = counters();
      const warming = native ? warmupCount : hit ? 1 : 0;
      for (let i = 0; i < warming; i++) {
        const name = group.kind.endsWith('miss') ? `warm${i}.${group.kind === 'cname_miss' ? 'aliases' : 'miss'}.fs-wire.test` : group.names[i % group.names.length];
        await request(64000 + i, name, blocked);
      }
      assert.equal(active.latencies.length, warming, `warmup ${group.name}: ${active.errors}`);
      assert.equal(active.unexpected, 0, 'unexpected warmup reply');
      const warmupUpstream = upstreamCount - warmupBefore;
      // The repeated alias may survive the diagnostic prelude (notably smoke).
      const repeatAlias = diagnose && group.name.startsWith('cname_cached') && group.name !== 'cname_cached_clean';
      const expectedWarmupUpstream = repeatAlias ? warmupUpstream : hit ? 1 : group.kind.endsWith('miss') ? warming : 0;
      if (repeatAlias) assert([0, 1].includes(warmupUpstream));
      assert.equal(warmupUpstream, expectedWarmupUpstream);
      const before = await metrics(adminPort, diagnose);
      const warmupChecks = {
        responses: active.attempted === warming && active.sent === warming && active.received === warming && active.latencies.length === warming,
        no_errors: !active.invalid && !active.timeouts && !active.send_errors && !active.unexpected && fixtureErrors.length === warmupErrors,
        cache: !native || (before.cache_hits - warmupMetrics.cache_hits === (hit ? warming - warmupUpstream : 0) && before.cache_misses - warmupMetrics.cache_misses === warmupUpstream),
        filtering: !native || (before.query_blocked - warmupMetrics.query_blocked === (group.kind === 'query_blocked' ? warming : 0) && before.response_blocked - warmupMetrics.response_blocked === (group.kind.startsWith('cname') ? warming : 0)),
      };
      assert(Object.values(warmupChecks).every(Boolean), `warmup path mismatch: ${group.name}`);
      if (profile && group.name === 'cname_cached_sample_1') {
        const sampler = spawn('/usr/bin/sample', [String(pid), '1', '1', '-file', path.join(dir, 'native-sample.txt')], { stdio: ['ignore', 'pipe', 'pipe'] });
        let output = '';
        sampler.stdout.on('data', bytes => { output += bytes; }); sampler.stderr.on('data', bytes => { output += bytes; });
        sampling = new Promise(resolve => { sampler.on('error', error => resolve({ error: error.message, output })); sampler.on('close', code => resolve({ exit_code: code, output })); });
      }
      active = counters(); const upstreamBefore = upstreamCount, ecsBefore = ecsCount, errorsBefore = fixtureErrors.length;
      const cgroupBefore = isolated ? await linuxUsage(constraints.hierarchy[0].directory) : null;
      const serverCpuBefore = diagnose ? serverCpu(pid) : null;
      const eventLoop = diagnose ? monitorEventLoopDelay({ resolution: 1 }) : null;
      eventLoop?.enable();
      const utilizationBefore = performance.eventLoopUtilization();
      let next = 0; const scheduleLags = [], cpuBefore = process.cpuUsage(), begin = performance.now();
      if (diagnose) { active.windowStart = begin; active.windowCpu = cpuBefore; }
      await Promise.all(Array.from({ length: mode.concurrency }, async () => {
        while (next < count) {
          const id = next++;
          const due = mode.offered_qps ? begin + id * 1000 / mode.offered_qps : undefined;
          if (due !== undefined) { while (performance.now() < due) await wait(Math.max(0, due - performance.now())); scheduleLags.push((performance.now() - due) * 1000); }
          await request(id, group.names[id], blocked, due);
          if (active.invalid || active.timeouts || active.send_errors || active.unexpected || fixtureErrors.length !== errorsBefore) break;
        }
      }));
      const end = performance.now(), elapsedMs = end - begin, cpu = process.cpuUsage(cpuBefore);
      const cgroupAfter = isolated ? await linuxUsage(constraints.hierarchy[0].directory) : null;
      const utilization = performance.eventLoopUtilization(utilizationBefore);
      eventLoop?.disable();
      const serverCpuAfter = diagnose ? serverCpu(pid) : null;
      const after = await metrics(adminPort, diagnose);
      // Allow observer delivery after timing; retain only events starting in this group.
      if (diagnose) await new Promise(resolve => setImmediate(resolve));
      const delta = Object.fromEntries(Object.keys(after).map(key => [key, after[key] - (before[key] ?? 0)]));
      active.latencies.sort((a, b) => a - b); active.scheduledLatencies.sort((a, b) => a - b); scheduleLags.sort((a, b) => a - b);
      const { latencies, scheduledLatencies, receiveLatencies, validationLatencies, windows, windowStart, windowCpu, ...counts } = active, expectedUpstream = group.kind.endsWith('miss') ? count : 0;
      const checks = {
        all_sent_received_valid: active.attempted === count && active.sent === count && active.received === count && latencies.length === count,
        no_errors: !active.invalid && !active.timeouts && !active.send_errors && !active.unexpected && fixtureErrors.length === errorsBefore,
        upstream: upstreamCount - upstreamBefore === expectedUpstream && ecsCount - ecsBefore === expectedUpstream && delta.upstream_operations === expectedUpstream,
        cache: delta.cache_hits === (hit ? count : 0) && delta.cache_misses === expectedUpstream,
        filtering: delta.query_blocked === (group.kind === 'query_blocked' ? count : 0) && delta.response_blocked === (group.kind.startsWith('cname') ? count : 0),
        no_server_drops: delta.udp_dropped === 0 && delta.dropped === 0,
        no_cgroup_memory_failure: !isolated || ['max', 'oom', 'oom_kill'].every(key => cgroupAfter.memory_events[key] === cgroupBefore.memory_events[key]),
        diagnostic_timer_counts: !diagnose || (delta.request_duration_count === count && delta.upstream_duration_count === expectedUpstream),
      };
      const record = { engine, round, mode: mode.name, concurrency: mode.concurrency, offered_qps: mode.offered_qps, group: group.name, count, ...counts,
        elapsed_ms: elapsedMs, sent_qps: active.sent / elapsedMs * 1000, completed_qps: latencies.length / elapsedMs * 1000,
        p50_us: percentile(latencies, .5), p95_us: percentile(latencies, .95), p99_us: percentile(latencies, .99),
        scheduled_p50_us: percentile(scheduledLatencies, .5), scheduled_p95_us: percentile(scheduledLatencies, .95), scheduled_p99_us: percentile(scheduledLatencies, .99),
        client_cpu_user_secs: cpu.user / 1e6, client_cpu_system_secs: cpu.system / 1e6, client_cpu_percent_one_core: (cpu.user + cpu.system) / (elapsedMs * 10),
        linux_cgroup: isolated ? { before: cgroupBefore, after: cgroupAfter, cpu_percent_one_core: (cgroupAfter.cpu.usage_usec - cgroupBefore.cpu.usage_usec) / (elapsedMs * 10), scope: 'service cgroup sample around group, including small sample-boundary scheduling overhead; memory.current is not process RSS' } : null,
        schedule_lag_p99_us: percentile(scheduleLags, .99), upstream: upstreamCount - upstreamBefore, upstream_ecs_verified: ecsCount - ecsBefore,
        warmup_requests: warming, warmup_upstream: warmupUpstream, warmup_checks: warmupChecks, metric_deltas: delta, fixture_errors: fixtureErrors.slice(errorsBefore), checks, correctness_passed: Object.values(checks).every(Boolean), fixture: dir,
        diagnostic: diagnose ? { receive_us: distribution(receiveLatencies), validation_us: distribution(validationLatencies), scheduled_us: distribution(scheduledLatencies), server_cpu_seconds: serverCpuAfter - serverCpuBefore,
          resolver_mean_us: delta.request_duration_sum * 1e6 / count, upstream_mean_us: expectedUpstream ? delta.upstream_duration_sum * 1e6 / expectedUpstream : null,
          event_loop: { ...utilization, delay_p99_ms: eventLoop.percentile(99) / 1e6, delay_max_ms: eventLoop.max / 1e6 },
          gc: gcEvents.filter(event => event.start >= begin && event.start < end).map(event => ({ start_ms: event.start - begin, duration_ms: event.duration })), windows, final_window: { count: active.received % 1024, elapsed_ms: end - windowStart },
        } : null };
      records.push(record); await writeFile(path.join(dir, `${group.name}.json`), JSON.stringify(record, null, 2), { mode: 0o600 });
      console.log(JSON.stringify({ engine, round, mode: mode.name, group: group.name, completed: latencies.length, qps: record.completed_qps, p99_us: record.p99_us, correctness_passed: record.correctness_passed }));
      assert(record.correctness_passed, `functional mismatch; stop performance judgment: ${dir}/${group.name}.json`);
    }
    if (sampling) {
      const result = await bounded(sampling, 10000, 'native sampler timeout');
      await writeFile(path.join(dir, 'sampler.json'), JSON.stringify(result));
      assert.equal(result.exit_code, 0, 'native sampler failed');
    }
    const constraintsAfter = isolated ? await verifyLinuxServer(pid, unit, serviceCpus, driverCpus) : null;
    if (isolated) await writeFile(path.join(dir, 'constraints-after.json'), JSON.stringify(constraintsAfter, null, 2), { mode: 0o600 });
    const stopping = performance.now(); process.kill(pid, 'SIGTERM'); const code = await bounded(exited, 15000, 'shutdown timeout'); stopped = true;
    const lifecycle = { engine, round, mode: mode.name, startup_ms: startupMs, stop_ms: performance.now() - stopping, exit_code: code, semantic_digest: semanticDigest, ...resource(stderr), constraints_before: constraints, constraints_after: constraintsAfter, fixture: dir };
    await writeFile(path.join(dir, 'lifecycle.json'), JSON.stringify(lifecycle, null, 2), { mode: 0o600 }); console.log(JSON.stringify({ event: 'lifecycle', ...lifecycle }));
    if (isolated) assert(lifecycle.max_rss_bytes > 0 && lifecycle.cpu_user_secs !== null && lifecycle.cpu_system_secs !== null, 'missing Linux resource measurements');
    assert.equal(code, 0);
  } finally {
    if (isolated) { process.off('SIGTERM', cancelOwnedUnit); process.off('SIGINT', cancelOwnedUnit); }
    try { socket.close(); } catch { /* startup may fail before bind */ }
    if (isolated && !stopped) {
      // systemd owns this service, so killing the systemd-run client process
      // group alone would leave the fixture running after an assertion fails.
      const state = spawnSync('systemctl', ['show', '--property=LoadState', '--value', unit], { encoding: 'utf8', timeout: 5000 });
      if (state.stdout?.trim() !== 'not-found') {
        const cleanup = spawnSync('sudo', ['-n', 'systemctl', 'stop', unit], { encoding: 'utf8', timeout: 15000 });
        if (cleanup.status !== 0) stderr += `\nOwned unit cleanup failed (${unit}): ${cleanup.error ?? cleanup.stderr}`;
      }
    }
    if (!stopped) { try { process.kill(-child.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; } await bounded(exited.catch(() => null), 2000, 'owned process group did not exit'); }
    await writeFile(path.join(dir, 'server.stdout'), stdout, { mode: 0o600 }); await writeFile(path.join(dir, 'server.stderr'), stderr, { mode: 0o600 });
  }
}
try {
  if (driverComparison) {
    for (let round = 1; round <= rounds; round++) for (const mode of round % 2 ? modes : [...modes].reverse()) await run('radix', round, mode);
  } else {
    for (const mode of modes) for (let round = 1; round <= rounds; round++) for (const engine of round % 2 ? ['trie', 'radix'] : ['radix', 'trie']) await run(engine, round, mode);
  }
} finally {
  gcObserver?.disconnect();
  upstream.close(); await writeFile(path.join(root, 'results.json'), JSON.stringify(records, null, 2), { mode: 0o600 });
}
console.log(JSON.stringify({ event: 'completed', root, smoke, scenarios: records.length, correctness_passed: records.every(record => record.correctness_passed) }));

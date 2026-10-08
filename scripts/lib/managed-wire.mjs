// Neutral wire load driver. Callers own workloads, state, authentication and actions.
import assert from 'node:assert/strict';
import dgram from 'node:dgram';
import { writeFile } from 'node:fs/promises';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { nameLabels, parse, query } from './wire-bench.mjs';

const IP = Buffer.from([192, 0, 2, 42]);
const fail = code => Object.assign(new Error(code), { code });
export function distribution(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const percentile = p => sorted.length ? sorted[Math.max(0, Math.ceil(sorted.length * p) - 1)] : null;
  return { count: sorted.length, p50: percentile(.5), p95: percentile(.95), p99: percentile(.99), max: sorted.at(-1) ?? null };
}
export function plannedCount(duration, rate) { return Math.round(duration * rate / 1000); }
export function dueCount(elapsed, duration, rate) {
  return Math.min(plannedCount(duration, rate), Math.max(0, Math.floor(elapsed * rate / 1000) + 1));
}
export function oracle(wire, packet, blocked) {
  const parsed = parse(wire);
  assert.equal(wire.readUInt16BE(0), packet.readUInt16BE(0), 'DNS ID');
  assert.equal(wire.readUInt16BE(2), 0x8180, 'DNS flags');
  assert(wire.subarray(12, parsed.questionEnd).equals(packet.subarray(12)), 'DNS question');
  assert.equal(wire.readUInt16BE(8), 0, 'DNS authority');
  assert.equal(wire.readUInt16BE(10), 0, 'DNS additional');
  assert.equal(parsed.options.length, 0, 'EDNS leak');
  assert.equal(parsed.answers.length, blocked ? 0 : 1, 'DNS answer count');
  if (!blocked) {
    const answer = parsed.answers[0];
    assert.equal(answer.type, 1); assert.equal(answer.klass, 1);
    assert(answer.data.equals(IP), 'DNS address');
    assert.deepEqual(answer.owner, nameLabels(packet, 12), 'DNS answer owner');
    assert(answer.ttl > 0 && answer.ttl <= 3600, 'DNS TTL');
  }
}
export function aResponse(packet) {
  const end = parse(packet).questionEnd;
  const response = Buffer.from(packet.subarray(0, end));
  response.writeUInt16BE(0x8180, 2); response.writeUInt16BE(1, 6);
  response.writeUInt16BE(0, 8); response.writeUInt16BE(0, 10);
  const rr = Buffer.from('c00c0001000100000e100004c000022a', 'hex');
  return Buffer.concat([response, rr]);
}
export async function probe(port, name, blocked, deadline = 2000) {
  const socket = dgram.createSocket('udp4'); const packet = query(12345, name);
  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(fail('probe_timeout')), deadline);
      socket.on('error', error => { clearTimeout(timer); reject(error); });
      socket.on('message', (wire, peer) => {
        clearTimeout(timer);
        try { assert.equal(peer.address, '127.0.0.1'); assert.equal(peer.port, port); oracle(wire, packet, blocked); resolve(); }
        catch (error) { reject(error); }
      });
      socket.send(packet, port, '127.0.0.1');
    });
  } finally { socket.close(); }
}
export function ioStat(text) {
  return Object.fromEntries(text.trim().split('\n').filter(Boolean).map(line => {
    const [device, ...fields] = line.split(/\s+/); assert.match(device, /^\d+:\d+$/);
    return [device, Object.fromEntries(fields.filter(field => /^(rbytes|wbytes|rios|wios)=/.test(field)).map(field => {
      const [key, value] = field.split('='); assert.match(value, /^\d+$/); return [key, Number(value)];
    }))];
  }));
}

export async function runWirePhase({ name, port, duration, output, slo: SLO, warmup, readMetrics, observeResources, itemFor, onAction, actionKey, safeCode, errorDiagnostic }) {
  await warmup();
  const warmBefore = await readMetrics();
  await warmup();
  const warmAfter = await readMetrics();
  assert.equal(warmAfter.cache_hits, warmBefore.cache_hits + 1, 'fresh-cache group must really hit cache');
  const socket = dgram.createSocket('udp4');
  await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
  const pending = new Map(); const expired = new Set(); let nextId = 0;
  const result = { name, duration_ms: duration, configured_qps: SLO.rate,
    planned: plannedCount(duration, SLO.rate), sent: 0, correct: 0, timeouts: 0,
    invalid: 0, late: 0, unexpected: 0, send_errors: 0, never_sent: 0,
    groups: {}, resource_samples: [], fresh_cache_probe_verified: true };
  const latencies = [], wireLatencies = [], lags = []; let sentCount = 0;
  let action, actionError, samplingError, samplingDone = false;
  let started;
  const expire = (id, entry) => {
    pending.delete(id); expired.add(id); result.timeouts++; entry.group.timeouts++;
    latencies.push(Math.max(SLO.client_deadline_ms, performance.now() - entry.due));
  };
  socket.on('message', (wire, peer) => {
    const id = wire.length >= 2 ? wire.readUInt16BE(0) : -1;
    const entry = pending.get(id);
    if (!entry) { if (expired.has(id)) result.late++; else result.unexpected++; return; }
    pending.delete(id); clearTimeout(entry.timer);
    try {
      assert.equal(peer.address, '127.0.0.1'); assert.equal(peer.port, port);
      oracle(wire, entry.packet, entry.blocked);
      const end = performance.now();
      if (end - entry.due > SLO.client_deadline_ms) {
        expired.add(id); result.timeouts++; entry.group.timeouts++;
      } else { result.correct++; entry.group.correct++; }
      latencies.push(end - entry.due); wireLatencies.push(end - entry.sent);
    } catch { result.invalid++; entry.group.invalid++; }
  });
  socket.on('error', () => { result.send_errors++; });
  try {
    const before = await observeResources(); result.resource_samples.push(before);
    started = performance.now();
    const sampler = (async () => {
      while (!samplingDone) {
        await delay(100);
        if (!samplingDone) result.resource_samples.push(await observeResources());
      }
    })().catch(error => { samplingError = error; });
    while (sentCount < result.planned) {
      const now = performance.now(); const elapsed = now - started;
      if (onAction && elapsed >= SLO.refresh_at_ms && !action) {
        action = onAction().then(value => { result[actionKey] = value; }).catch(error => { actionError = error; });
      }
      const due = dueCount(elapsed, duration, SLO.rate);
      while (sentCount < due) {
        const sequence = sentCount++; const item = itemFor(sequence, name);
        const plannedAt = started + sequence * 1000 / SLO.rate;
        const sent = performance.now(); lags.push(sent - plannedAt);
        const group = result.groups[item.group] ??= { planned: 0, correct: 0, timeouts: 0, invalid: 0 };
        group.planned++;
        if (sent - plannedAt >= SLO.client_deadline_ms) {
          result.never_sent++; result.timeouts++; group.timeouts++; latencies.push(sent - plannedAt); continue;
        }
        // No live ID is reused; the name/question additionally binds reused IDs.
        while (pending.has(nextId)) nextId = (nextId + 1) & 65535;
        const id = nextId; nextId = (nextId + 1) & 65535; expired.delete(id);
        const packet = query(id, item.name); const entry = { packet, blocked: item.blocked, group, due: plannedAt, sent };
        pending.set(id, entry); result.sent++;
        entry.timer = setTimeout(() => expire(id, entry), Math.max(1, plannedAt + SLO.client_deadline_ms - performance.now()));
        socket.send(packet, port, '127.0.0.1', error => { if (error) result.send_errors++; });
      }
      if (sentCount < result.planned) await delay(1);
    }
    // Fixed duration, not last-response time, is the QPS denominator.
    await delay(Math.max(0, started + duration - performance.now()));
    while (pending.size) await delay(5);
    if (action) await action;
    samplingDone = true; await sampler;
    result.resource_samples.push(await observeResources());
    const after = result.resource_samples.at(-1);
    const finalMetrics = await readMetrics();
    result.metric_deltas = Object.fromEntries(['query_received', 'query_blocked', 'cache_hits', 'cache_misses', 'upstream_success', 'upstream_failure']
      .filter(key => Object.hasOwn(warmAfter, key)).map(key => [key, finalMetrics[key] - warmAfter[key]]));
    result.actual_elapsed_ms = performance.now() - started;
    result.correct_qps = result.correct / (duration / 1000);
    result.timeout_fraction = result.timeouts / result.planned;
    result.scheduled_latency_ms = distribution(latencies);
    result.wire_latency_ms = distribution(wireLatencies);
    result.driver_lag_ms = distribution(lags);
    result.cpu_seconds = (after.cpu.usage_usec - before.cpu.usage_usec) / 1e6;
    result.cpu_average_cores = result.cpu_seconds / (result.actual_elapsed_ms / 1000);
    result.rss_sample_peak_bytes = Math.max(...result.resource_samples.map(s => s.rss_bytes));
    result.process_lifetime_rss_peak_bytes = after.process_lifetime_rss_peak_bytes;
    result.cgroup_lifetime_peak_bytes = after.cgroup_lifetime_peak_bytes;
    result.oom_events = Object.fromEntries(['oom', 'oom_kill', 'oom_group_kill'].map(key => [key, after.memory_events[key] ?? 0]));
    const update = Boolean(onAction);
    result.failures = [];
    const check = (condition, code) => { if (!condition) result.failures.push(code); };
    check(result.invalid === 0 && result.unexpected === 0 && result.send_errors === 0, 'correctness');
    check(result.correct + result.timeouts + result.invalid === result.planned, 'sample_accounting');
    check(result.timeout_fraction <= SLO.timeout_fraction, 'timeout_fraction');
    check(result.correct_qps >= SLO.correct_qps, 'offered_load_correct_qps');
    check(result.scheduled_latency_ms.p99 !== null && result.scheduled_latency_ms.p99 <= (update ? SLO.update_p99_ms : SLO.steady_p99_ms), 'scheduled_p99');
    check(update ? result.process_lifetime_rss_peak_bytes <= SLO.update_rss_bytes : result.rss_sample_peak_bytes <= SLO.steady_rss_bytes, 'rss');
    check(Object.values(result.oom_events).every(value => value === 0), 'oom');
    check(!samplingError, 'resource_sampling'); check(!actionError, `${actionKey}_evidence`);
    check(!onAction || Boolean(result[actionKey]), `${actionKey}_completed`);
    if (actionError) {
      result[`${actionKey}_error`] = safeCode(actionError.code);
      if (errorDiagnostic) result[`${actionKey}_http`] = errorDiagnostic(actionError);
    }
    result.result = result.failures.length ? 'failed' : 'passed';
    await writeFile(path.join(output, `${name}.json`), JSON.stringify(result, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
    console.log(JSON.stringify({ event: 'phase_complete', name, result: result.result,
      correct_qps: result.correct_qps, scheduled_p99_ms: result.scheduled_latency_ms.p99, failures: result.failures }));
    return result;
  } finally {
    samplingDone = true;
    for (const entry of pending.values()) clearTimeout(entry.timer);
    socket.close();
  }
}


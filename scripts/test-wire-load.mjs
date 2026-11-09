import assert from 'node:assert/strict';
import dgram from 'node:dgram';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { cpuSeconds, options } from './bench-dns.mjs';
import { aResponse, runWireLoad, runWirePhase } from './lib/managed-wire.mjs';

test('ps CPU times retain centiseconds and Linux day/hour fields', () => {
  assert.equal(cpuSeconds('0:00.23'), .23);
  assert.equal(cpuSeconds('02:03'), 123);
  assert.equal(cpuSeconds('01:02:03'), 3723);
  assert.equal(cpuSeconds('2-01:02:03'), 176523);
  assert.throws(() => cpuSeconds('unknown'));
});
test('baseline options reject unbounded loads and unknown cases', () => {
  const base = ['--binary', '/unused', '--output', '/unused-output'];
  assert.equal(options(base).workers, 4);
  assert.throws(() => options([...base, '--rates', '0']));
  assert.throws(() => options([...base, '--seconds', '999']));
  assert.throws(() => options([...base, '--cases', 'production']));
  assert.throws(() => options([...base, '--unknown', 'x']));
});
async function fixture(action) {
  const socket = dgram.createSocket('udp4');
  socket.on('message', (packet, peer) => {
    const answer = aResponse(packet);
    if (action === 'wrong-address') answer[answer.length - 1] = 99;
    socket.send(answer, peer.port, peer.address);
  });
  await new Promise(resolve => socket.bind(0, '127.0.0.1', resolve));
  return { port: socket.address().port, close: () => new Promise(resolve => socket.close(resolve)) };
}
function settings(port) {
  let warmed = 0;
  return { name: 'unit', port, duration: 100, slo: { rate: 200, client_deadline_ms: 1000 },
    warmup: async () => { warmed++; }, readMetrics: async () => ({ cache_hits: warmed }),
    resourceIntervalMs: 10, observeResources: async () => ({ portable: true }),
    itemFor: i => ({ name: `q${i}.test`, group: 'answer', blocked: false }) };
}
test('wire core checks every actual UDP answer with caller-owned resource interpretation', async () => {
  const peer = await fixture();
  try {
    const row = await runWireLoad(settings(peer.port), async (result, context) => {
      assert.equal(context.samplingError, undefined);
      assert.deepEqual(context.before, { portable: true });
      return result;
    });
    assert.equal(row.planned, 20); assert.equal(row.correct, 20);
    assert.equal(row.correct_qps, 200); assert.equal(row.invalid, 0);
    assert.equal(row.wire_latency_ms.count, 20);
    assert.equal(row.fresh_cache_probe_verified, true);
    assert.equal(row.oom_events, undefined, 'portable core must not invent Linux evidence');
  } finally { await peer.close(); }
});
test('incorrect wire answers fail correctness rather than counting as throughput', async () => {
  const peer = await fixture('wrong-address');
  try {
    const row = await runWireLoad({ ...settings(peer.port), expectCacheHit: false }, async result => result);
    assert.equal(row.correct, 0); assert.equal(row.invalid, 20);
    assert.equal(row.correct_qps, 0); assert.equal(row.fresh_cache_probe_verified, false);
  } finally { await peer.close(); }
});
test('managed wrapper retains Linux resource gates, action evidence and JSON format', async () => {
  const peer = await fixture();
  const output = await mkdtemp(path.join(os.tmpdir(), 'parins-wire-gate-'));
  try {
    const options = settings(peer.port);
    const row = await runWirePhase({ ...options, output,
      slo: { ...options.slo, refresh_at_ms: 10, timeout_fraction: 0, correct_qps: 200,
        update_p99_ms: 1000, steady_p99_ms: 1000, steady_rss_bytes: 1024, update_rss_bytes: 1024 },
      observeResources: async () => ({ cpu: { usage_usec: 0 }, rss_bytes: 100,
        process_lifetime_rss_peak_bytes: 100, cgroup_lifetime_peak_bytes: 100,
        memory_events: { oom_kill: 1 } }),
      onAction: async () => ({ committed: true }), actionKey: 'change', safeCode: String });
    assert.equal(row.result, 'failed'); assert.deepEqual(row.failures, ['oom']);
    assert.deepEqual(row.change, { committed: true });
    assert.equal(row.oom_events.oom_kill, 1);
    assert.deepEqual(JSON.parse(await readFile(path.join(output, 'unit.json'), 'utf8')), row);
  } finally { await peer.close(); await rm(output, { recursive: true }); }
});

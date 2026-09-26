// Shared wire fixture helpers from bench-runtime.mjs; parse also exposes owner/TTL.
import assert from 'node:assert/strict';
import http from 'node:http';
import { platform } from 'node:os';

export function query(id, name) {
  const labels = name.split('.').flatMap(s => [Buffer.from([s.length]), Buffer.from(s)]);
  const header = Buffer.alloc(12);
  header.writeUInt16BE(id, 0); header.writeUInt16BE(0x100, 2); header.writeUInt16BE(1, 4);
  return Buffer.concat([header, ...labels, Buffer.from([0, 0, 1, 0, 1])]);
}
export function nameEnd(wire, at) {
  while (at < wire.length) {
    const size = wire[at++];
    if (!size) return at;
    if ((size & 0xc0) === 0xc0) { assert(at < wire.length, 'short DNS pointer'); return at + 1; }
    assert(size <= 63 && at + size <= wire.length, 'invalid DNS label'); at += size;
  }
  throw new Error('missing DNS name terminator');
}
// Fixture assertion helper; bounded compression traversal for answer ownership.
export function nameLabels(wire, at) {
  const labels = []; let decodedBytes = 1;
  for (let hops = 0; hops < wire.length; hops++) {
    assert(at < wire.length, 'short DNS name');
    const size = wire[at++];
    if (!size) return labels;
    if ((size & 0xc0) === 0xc0) {
      assert(at < wire.length, 'short DNS pointer'); at = ((size & 0x3f) << 8) | wire[at]; continue;
    }
    assert(size <= 63 && at + size <= wire.length, 'invalid DNS label');
    decodedBytes += size + 1; assert(decodedBytes <= 255, 'oversized DNS name');
    labels.push(wire.subarray(at, at + size)); at += size;
  }
  throw new Error('DNS pointer loop');
}
export function parse(wire) {
  assert(wire.length >= 12 && wire.readUInt16BE(4) === 1, 'one question required');
  const questionEnd = nameEnd(wire, 12) + 4;
  assert(questionEnd <= wire.length, 'short question');
  let at = questionEnd;
  const answers = [], options = [];
  for (let section = 0; section < 3; section++) for (let i = 0; i < wire.readUInt16BE(6 + section * 2); i++) {
    const owner = section === 0 ? nameLabels(wire, at) : undefined;
    at = nameEnd(wire, at); assert(at + 10 <= wire.length, 'short RR');
    const type = wire.readUInt16BE(at), klass = wire.readUInt16BE(at + 2), length = wire.readUInt16BE(at + 8);
    const ttl = wire.readUInt32BE(at + 4);
    const data = wire.subarray(at + 10, at + 10 + length); assert.equal(data.length, length, 'short RDATA');
    if (section === 0) answers.push({ type, klass, data, owner, ttl });
    if (type === 41) for (let off = 0; off < data.length;) {
      assert(off + 4 <= data.length, 'short EDNS option');
      const code = data.readUInt16BE(off), size = data.readUInt16BE(off + 2);
      assert(off + 4 + size <= data.length, 'short EDNS payload');
      options.push({ code, data: data.subarray(off + 4, off + 4 + size) }); off += 4 + size;
    }
    at += 10 + length;
  }
  assert.equal(at, wire.length, 'trailing DNS bytes');
  return { questionEnd, answers, options };
}
export function bounded(promise, ms, label) {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(label)), ms); })]).finally(() => clearTimeout(timer));
}
export function resource(stderr) {
  const number = pattern => { const value = stderr.match(pattern)?.[1]; return value === undefined ? null : Number(value); };
  const darwin = platform() === 'darwin';
  const rss = number(darwin ? /(\d+)\s+maximum resident set size/ : /Maximum resident set size \(kbytes\):\s*(\d+)/);
  return {
    resource_scope: 'whole server process including startup/shutdown; excludes load generator',
    cpu_user_secs: number(darwin ? /([\d.]+) user/ : /User time \(seconds\):\s*([\d.]+)/),
    cpu_system_secs: number(darwin ? /([\d.]+) sys/ : /System time \(seconds\):\s*([\d.]+)/),
    max_rss_bytes: rss === null ? null : rss * (darwin ? 1 : 1024),
    block_output_operations: darwin ? number(/(\d+)\s+block output operations/) : null,
    linux_file_system_outputs: platform() === 'linux' ? number(/File system outputs:\s*(\d+)/) : null,
  };
}
export function metrics(port, includeTimers = false) {
  return new Promise((resolve, reject) => {
    const request = http.get({ hostname: '127.0.0.1', port, path: '/metrics', agent: false }, response => {
      let body = '';
      response.on('data', chunk => { body += chunk; if (body.length > 1024 * 1024) request.destroy(new Error('metrics too large')); });
      response.on('end', () => { try {
        assert.equal(response.statusCode, 200);
        const values = Object.fromEntries([...body.matchAll(/^parins_([a-z_]+)_total (\d+)$/gm)].map(match => [match[1], Number(match[2])]));
        if (includeTimers) for (const match of body.matchAll(/^parins_(request|upstream)_duration_seconds_(count|sum) ([\d.]+)$/gm)) values[`${match[1]}_duration_${match[2]}`] = Number(match[3]);
        resolve(values);
      } catch (error) { reject(error); } });
      response.on('error', reject);
    });
    request.setTimeout(2000, () => request.destroy(new Error('metrics timeout'))); request.on('error', reject);
  });
}

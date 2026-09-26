// Shared wire fixture helpers from bench-runtime.mjs; parse also exposes owner/TTL.
import assert from 'node:assert/strict';
import http from 'node:http';
import { platform } from 'node:os';
import { readFile, readdir } from 'node:fs/promises';
import path from 'node:path';

export function cpuList(value) {
  assert(typeof value === 'string' && /^\d+(?:-\d+)?(?:,\d+(?:-\d+)?)*$/.test(value), 'invalid CPU list');
  const cpus = [];
  for (const range of value.split(',')) {
    const [first, end = first] = range.split('-').map(Number);
    assert(Number.isSafeInteger(end) && end >= first && end < 65536, 'invalid CPU range');
    for (let cpu = first; cpu <= end; cpu++) cpus.push(cpu);
  }
  assert.equal(new Set(cpus).size, cpus.length, 'overlapping CPU ranges');
  return cpus.sort((a, b) => a - b);
}

// Evidence for the dedicated hosted-runner fixture, not a portable launcher.
export async function linuxConstraints(pid) {
  const status = await readFile(`/proc/${pid}/status`, 'utf8');
  const cgroup = await readFile(`/proc/${pid}/cgroup`, 'utf8');
  const relative = cgroup.match(/^0::(\/[^\n]*)$/m)?.[1];
  assert(relative !== undefined && !relative.split('/').includes('..'), 'cgroup v2 is required');
  const root = '/sys/fs/cgroup';
  const directory = path.join(root, relative);
  const hierarchy = [];
  for (let current = directory; current !== root; current = path.dirname(current)) {
    assert(current.startsWith(`${root}/`), 'invalid cgroup path');
    const files = ['cpuset.cpus.effective', 'cpu.max', 'cpu.stat', 'memory.max', 'memory.swap.max', 'memory.current', 'memory.peak', 'memory.events'];
    const values = Object.fromEntries(await Promise.all(files.map(async name => {
      try { return [name, (await readFile(path.join(current, name), 'utf8')).trim()]; }
      catch (error) { if (error.code === 'ENOENT' && name === 'memory.peak') return [name, null]; throw error; }
    })));
    hierarchy.push({ directory: current, ...values });
  }
  return { pid, allowed_cpus: status.match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1], cgroup: relative, hierarchy };
}

export async function verifyLinuxServer(pid, unit, serviceCpus, driverCpus) {
  const snapshot = await linuxConstraints(pid);
  assert(snapshot.cgroup.endsWith(`/${unit}`), 'fixture is outside its owned transient unit');
  assert.deepEqual(cpuList(snapshot.allowed_cpus), serviceCpus, 'server affinity differs from the fixed two CPUs');
  const own = snapshot.hierarchy[0];
  assert(own, 'missing fixture cgroup');
  assert.deepEqual(cpuList(own['cpuset.cpus.effective']), serviceCpus, 'effective service cpuset differs');
  assert.equal(own['memory.max'], '4294967296', 'service must have a 4 GiB memory limit');
  assert.equal(own['memory.swap.max'], '0', 'service swap must be disabled');
  for (const level of snapshot.hierarchy) {
    const [quota, period] = level['cpu.max'].split(/\s+/);
    assert(quota === 'max' || Number(quota) / Number(period) >= 2, 'ancestor CPU quota is below two CPUs');
    assert(level['memory.max'] === 'max' || BigInt(level['memory.max']) >= 4294967296n, 'ancestor memory limit is below 4 GiB');
  }
  const tasks = await readdir(`/proc/${pid}/task`);
  for (const tid of tasks) {
    try {
      const status = await readFile(`/proc/${pid}/task/${tid}/status`, 'utf8');
      assert.deepEqual(cpuList(status.match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1]), serviceCpus, `thread ${tid} affinity differs`);
    } catch (error) { if (error.code !== 'ENOENT') throw error; }
  }
  const driver = await linuxConstraints('self');
  assert.deepEqual(cpuList(driver.allowed_cpus), driverCpus, 'driver affinity differs');
  assert(serviceCpus.every(cpu => !driverCpus.includes(cpu)), 'driver and service CPUs overlap');
  return snapshot;
}

export async function linuxUsage(directory) {
  const [cpu, memory, events] = await Promise.all(['cpu.stat', 'memory.current', 'memory.events'].map(name => readFile(path.join(directory, name), 'utf8')));
  const counters = text => Object.fromEntries(text.trim().split('\n').map(line => { const [name, value] = line.split(/\s+/); return [name, Number(value)]; }));
  return { cpu: counters(cpu), memory_current_bytes: Number(memory), memory_events: counters(events) };
}

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

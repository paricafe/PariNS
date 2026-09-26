import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const targets = ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl'];
const shared = ['version', 'source_commit', 'update_protocol', 'helper_protocol', 'install_contract',
  'durable_contract_epoch', 'runtime_database_format', 'cache_snapshot_format', 'cache_semantics'];
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const assert = (value, message) => { if (!value) throw new Error(message); };

export function createManifest(entries, { official = true, sourceCommit } = {}) {
  assert(entries.length === 2, 'Exactly two native artifacts required');
  const first = entries[0].build;
  assert(/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(first.version), 'Invalid version');
  assert(first.version.split('.').every(part => part.length <= 20 && BigInt(part) <= 18446744073709551615n), 'Version component overflow');
  assert(/^[0-9a-f]{40}$/.test(first.source_commit), 'Invalid source commit');
  if (sourceCommit) assert(first.source_commit === sourceCommit, 'Build/source SHA mismatch');
  assert(first.install_contract === 'linux-managed-updater-v1', 'Unsupported install contract');
  for (const key of shared.filter(key => !['version', 'source_commit', 'install_contract'].includes(key))) {
    assert(Number.isSafeInteger(first[key]) && first[key] > 0, `Invalid build field ${key}`);
  }
  const artifacts = targets.map(target => {
    const matches = entries.filter(e => e.build.target === target);
    assert(matches.length === 1, `Missing or duplicate ${target}`);
    const { build, size, sha256, name } = matches[0];
    assert(build.official_release === official, 'Build official marker mismatch');
    assert(shared.every(key => build[key] === first[key]), 'Native build contract mismatch');
    const arch = target.split('-')[0];
    assert(name === `parins-v${first.version}-linux-${arch}.bin`, 'Unexpected raw artifact name');
    assert(Number.isSafeInteger(size) && size >= 64 && size <= 128 * 1024 * 1024, 'Invalid artifact size');
    assert(/^[0-9a-f]{64}$/.test(sha256), 'Invalid artifact SHA256');
    return { target, name, size, sha256 };
  });
  return {
    schema: 1, repository: 'paricafe/PariNS', version: first.version, tag: `v${first.version}`,
    source_commit: first.source_commit, update_protocol: first.update_protocol,
    install_contract: first.install_contract, min_helper_protocol: first.helper_protocol,
    durable_contract_epoch: first.durable_contract_epoch, runtime_database_format: first.runtime_database_format,
    cache_snapshot_format: first.cache_snapshot_format, cache_semantics: first.cache_semantics,
    upgrade_mode: 'in_place', artifacts,
  };
}

export function verifyPackages(directory, version, sourceCommit, official) {
  assert(/^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version), 'Invalid tag');
  const entries = targets.map(target => {
    const arch = target.split('-')[0];
    const base = `parins-${version}-linux-${arch}`;
    const raw = join(directory, `${base}.bin`);
    const size = statSync(raw).size;
    assert(size >= 64 && size <= 128 * 1024 * 1024, 'Raw ELF size exceeds contract');
    const bytes = readFileSync(raw);
    assert(bytes.subarray(0, 6).equals(Buffer.from('7f454c460201', 'hex')), 'Not ELF64 little-endian');
    assert(bytes.readUInt16LE(18) === (arch === 'x86_64' ? 62 : 183), 'ELF architecture mismatch');
    const headers = execFileSync('readelf', ['--program-headers', raw], { encoding: 'utf8' });
    const dynamic = execFileSync('readelf', ['--dynamic', raw], { encoding: 'utf8' });
    assert(!/INTERP/.test(headers) && !/NEEDED/.test(dynamic), 'ELF must be static');
    const archive = join(directory, `${base}.tar.gz`);
    const packed = execFileSync('tar', ['-xOzf', archive, `${base}/parins`], { maxBuffer: 128 * 1024 * 1024 });
    assert(bytes.equals(packed), 'Raw/package ELF mismatch');
    const packedInfo = execFileSync('tar', ['-xOzf', archive, `${base}/install-build-info.json`], { encoding: 'utf8', maxBuffer: 8192 });
    const build = JSON.parse(readFileSync(join(directory, `${base}.build-info.json`), 'utf8'));
    assert(JSON.stringify(JSON.parse(packedInfo)) === JSON.stringify(build), 'Package build-info mismatch');
    assert(`v${build.version}` === version, 'Package/tag version mismatch');
    return { build, size, sha256: hash(bytes), name: `${base}.bin` };
  });
  return createManifest(entries, { official, sourceCommit });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const [directory, version, sourceCommit, mode] = process.argv.slice(2);
    assert(directory && version && /^[0-9a-f]{40}$/.test(sourceCommit ?? '') && ['official', 'development'].includes(mode),
      'Usage: node scripts/update-manifest.mjs DIRECTORY vX.Y.Z SOURCE_SHA official|development');
    const manifest = verifyPackages(resolve(directory), version, sourceCommit, mode === 'official');
    writeFileSync(join(directory, 'parins-update.json'), `${JSON.stringify(manifest, null, 2)}\n`, { flag: 'wx' });
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}

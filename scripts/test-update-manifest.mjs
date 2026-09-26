import test from 'node:test';
import assert from 'node:assert/strict';
import { createManifest } from './update-manifest.mjs';

function fixtures() {
  return ['x86_64', 'aarch64'].map(arch => ({
    build: { version: '0.1.4', target: `${arch}-unknown-linux-musl`, source_commit: 'a'.repeat(40), official_release: true,
      update_protocol: 1, helper_protocol: 1, install_contract: 'linux-managed-updater-v1',
      durable_contract_epoch: 1, runtime_database_format: 2, cache_snapshot_format: 2, cache_semantics: 2 },
    name: `parins-v0.1.4-linux-${arch}.bin`, size: 100, sha256: 'b'.repeat(64),
  }));
}

test('combined manifest is derived from matching native build contracts', () => {
  const result = createManifest(fixtures(), { sourceCommit: 'a'.repeat(40) });
  assert.equal(result.version, '0.1.4');
  assert.equal(result.artifacts.length, 2);
  assert.equal(result.runtime_database_format, 2);
  assert.equal(result.durable_contract_epoch, 1);
  assert.equal(result.upgrade_mode, 'in_place');
});

test('missing, duplicate, inconsistent or unofficial builds fail closed', () => {
  for (const mutate of [
    e => e.pop(), e => { e[1] = structuredClone(e[0]); },
    e => { e[0].build.official_release = false; }, e => { e[1].build.source_commit = 'c'.repeat(40); },
    e => { e[1].build.durable_contract_epoch += 1; }, e => { e[1].size = 0; },
    e => { e[0].size = 129 * 1024 * 1024; }, e => { e[0].sha256 = 'g'.repeat(64); },
    e => { e[0].name = '../parins'; }, e => { e[0].build.version = '0.01.4'; },
  ]) { const entries = fixtures(); mutate(entries); assert.throws(() => createManifest(entries)); }
  assert.throws(() => createManifest(fixtures(), { sourceCommit: 'c'.repeat(40) }));
});

test('development artifacts require explicit non-official validation', () => {
  const entries = fixtures();
  entries.forEach(e => { e.build.official_release = false; });
  assert.throws(() => createManifest(entries));
  assert.equal(createManifest(entries, { official: false }).artifacts.length, 2);
});

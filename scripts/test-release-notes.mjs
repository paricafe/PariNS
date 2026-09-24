import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { releaseNotes } from './release-notes.mjs';

const changelog = '# Changelog\n\n## Unreleased\n\nFuture\n\n## v0.1.4 — 2026-09-24\n\n### Added\n\n- Current\n\n## v0.1.3 — 2026-09-24\n\n- Previous\n';
test('selects only the exact current version and retains subsection headings', () => {
  assert.equal(releaseNotes(changelog, 'v0.1.4'), '### Added\n\n- Current\n');
  assert.equal(releaseNotes(changelog, 'v0.1.3'), '- Previous\n');
  assert.equal(releaseNotes(changelog.replaceAll('\n', '\r\n'), 'v0.1.4'), '### Added\r\n\r\n- Current\n');
});
test('missing, duplicate, invalid and empty sections stop publication', () => {
  for (const version of ['v0.1.40', '0.1.4', 'v0.1.4-rc.1', undefined]) assert.throws(() => releaseNotes(changelog, version));
  assert.throws(() => releaseNotes(changelog + '\n## v0.1.4\n- Duplicate', 'v0.1.4'));
  assert.throws(() => releaseNotes('## v0.1.4\n\n## v0.1.3\n- Old', 'v0.1.4'));
});
test('the package version has release notes without another version section', () => {
  const cargo = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8');
  const version = `v${cargo.match(/^version = "([^"]+)"$/m)[1]}`;
  const notes = releaseNotes(readFileSync(new URL('../CHANGELOG.md', import.meta.url), 'utf8'), version);
  assert(notes.length > 100);
  assert(!/^## /m.test(notes));
});

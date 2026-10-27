import { describe, expect, it } from 'vitest';
import { hasTranslation } from '../index';
import httpsReader from '../../../../src/https_reader.rs?raw';
import updateReader from '../../../../src/update/reader.rs?raw';
import contract from '../../../../src/update/contract.rs?raw';
import executor from '../../../../src/update/executor/mod.rs?raw';
import journal from '../../../../src/update/executor/journal.rs?raw';
import ipc from '../../../../src/update/ipc.rs?raw';
import coordinator from '../../../../src/manage/updates.rs?raw';
import operation from '../../../../src/manage/updates/operation.rs?raw';
import state from '../../../../src/manage/updates/state.rs?raw';

// The update panel shows check errors, operation phases and reasons, capability
// reasons and update API error codes through `updates.<code>`. They come from
// these Rust sources as snake_case string literals, so every such literal outside
// test modules needs a translation unless it is listed below.
const SOURCES = { httpsReader, updateReader, contract, executor, journal, ipc, coordinator, operation, state };
const NOT_SHOWN = new Set([
  'snake_case', // serde attribute
  'https', // URI scheme
  'rule_files', 'upstream_ca', // preflight input categories, logged only
  'operation_missing', 'operation_changed', 'launch_identity_mismatch', // internal errors; the API reports update_state_unavailable
]);

function productionCode(source: string): string {
  const tests = source.search(/#\[cfg\(test\)\]\s*mod \w+ \{/);
  return (tests < 0 ? source : source.slice(0, tests)).replace(/^\s*\/\/.*$/gm, '');
}

function codes(): Set<string> {
  const found = new Set<string>();
  for (const source of Object.values(SOURCES).map(productionCode)) {
    // Literals followed by ':' are JSON keys in `json!` views.
    for (const [, code] of source.matchAll(/"([a-z]+(?:_[a-z0-9]+)+)"(?!\s*:)/g)) found.add(code);
    // One-word reasons and phases.
    for (const [, a, b, c] of source.matchAll(/(?:Some|Err)\(\s*"([a-z]+)"|\|_\| "([a-z]+)"|set_phase\([^,]+, "([a-z]+)"/g)) found.add(a ?? b ?? c);
  }
  const phases = /pub enum Phase \{([^}]*)\}/.exec(productionCode(ipc))?.[1] ?? '';
  for (const [variant] of phases.matchAll(/[A-Z]\w*/g)) found.add(variant.replace(/(?<!^)[A-Z]/g, (c) => `_${c}`).toLowerCase());
  for (const code of NOT_SHOWN) found.delete(code);
  return found;
}

describe('update codes', () => {
  it('translates every update and reader code that can reach the console', () => {
    const all = codes();
    expect(all.size).toBeGreaterThan(40);
    expect(all).toContain('destination_not_allowed');
    expect(all).toContain('rolling_back');
    expect([...all].filter((code) => !hasTranslation(`updates.${code}`))).toEqual([]);
  });
});

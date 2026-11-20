import { expect, it } from 'vitest';
import { hasTranslation } from '../index';

const management = import.meta.glob<string>(
  ['../../../../src/manage/**/*.rs', '!../../../../src/manage/**/*tests.rs'],
  { query: '?raw', import: 'default', eager: true },
);

it('translates literal error codes returned by the management API', () => {
  const codes = new Set<string>();
  for (const source of Object.values(management)) {
    for (const [, code] of source.matchAll(/\berror\(\s*[^,()]+,\s*"([^"]+)"/g)) {
      codes.add(code);
    }
  }
  expect(codes).toContain('BODY_TIMEOUT');
  expect(codes).toContain('LIVE_LIMIT');
  expect([...codes].filter(code => !hasTranslation(`api.${code}`) && !hasTranslation(`subscriptions.${code}`))).toEqual([]);
});

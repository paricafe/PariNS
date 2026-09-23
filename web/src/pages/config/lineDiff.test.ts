import { describe, expect, it } from 'vitest';
import { lineDiff } from './lineDiff';

describe('configuration preview', () => {
  it('shows separate additions and removals with unchanged context', () => {
    expect(lineDiff('a\nb\nc\nd', 'a\nB\nc\ne\nd')).toEqual([
      { kind: 'same', text: 'a' },
      { kind: 'remove', text: 'b' },
      { kind: 'add', text: 'B' },
      { kind: 'same', text: 'c' },
      { kind: 'add', text: 'e' },
      { kind: 'same', text: 'd' },
    ]);
  });
});

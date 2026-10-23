import { describe, expect, it } from 'vitest';
import { restoreSummary } from './SettingsPage';

const report = (restored: number, skipped: number, reason: string | null) => ({ saved: restored + skipped, restored, skipped, bytes: 0, reason });

describe('cache restore summary', () => {
  it('keeps a restore that ran out of time apart from an unusable snapshot', () => {
    expect(restoreSummary(report(1200, 300, 'deadline'), 'en')).toBe('Restored the 1,200 most recently used entries at start; 300 more did not fit in the restore time limit');
    expect(restoreSummary(report(1200, 300, 'deadline'), 'zh-CN')).toBe('启动时恢复了最近用过的 1,200 条；其余 300 条超出恢复时限，未装入');
    expect(restoreSummary(report(4, 0, null), 'en')).toBe('Restored 4 entries at start; skipped 0');
    expect(restoreSummary(report(0, 0, 'snapshot checksum mismatch'), 'en')).toBe("The saved snapshot couldn't be used, so the cache started empty");
  });
});

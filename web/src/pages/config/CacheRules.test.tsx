// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { CacheRuleDraft } from '../../model';
import { CacheRules } from './SettingsPage';

const rule = (name: string): CacheRuleDraft => ({ name, suffix: 'false', qtype: '', bypass: 'false', max_ttl_secs: '', negative_ttl_cap_secs: '', prefetch: '', stale: '' });
const mocks = vi.hoisted(() => ({ config: { locked: false, draft: { settings: {}, rules: [] as unknown[] }, setRules: vi.fn() } }));
vi.mock('../../config/context', () => ({ useConfig: () => mocks.config }));
afterEach(cleanup);

describe('cache rules', () => {
  it('names the move buttons by rule number and removes the chosen row', async () => {
    mocks.config.draft.rules = [rule('a.test'), rule('b.test'), rule('c.test')];
    const view = render(<CacheRules language="en" />);
    mocks.config.setRules.mockImplementation((rules: CacheRuleDraft[]) => { mocks.config.draft.rules = rules; view.rerender(<CacheRules language="en" />); });
    expect(screen.getByRole('button', { name: 'Move rule 1 up' })).toHaveProperty('disabled', true);
    expect(screen.getByRole('button', { name: 'Move rule 2 down' })).toHaveProperty('disabled', false);
    const third = screen.getByDisplayValue('c.test');
    fireEvent.click(screen.getAllByRole('button', { name: 'Remove rule' })[1]);
    await waitFor(() => expect(screen.queryByDisplayValue('b.test')).toBeNull());
    // The third row keeps its element: only the removed row left the list.
    expect(screen.getByDisplayValue('c.test')).toBe(third);
    expect(screen.getByRole('button', { name: 'Move rule 2 up' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Move rule 3 up' })).toBeNull();
  });
});

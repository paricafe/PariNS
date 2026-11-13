// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { ModelError, type CacheRuleDraft } from '../../model';
import { CacheRules } from './SettingsPage';

const rule = (name: string): CacheRuleDraft => ({ name, suffix: 'false', qtype: '', bypass: 'false', max_ttl_secs: '', negative_ttl_cap_secs: '', prefetch: '', stale: '' });
const mocks = vi.hoisted(() => ({ config: { locked: false, error: null as unknown, draft: { settings: {}, rules: [] as unknown[] }, setRules: vi.fn() } }));
vi.mock('../../config/context', () => ({ useConfig: () => mocks.config }));
afterEach(() => { cleanup(); mocks.config.error = null; mocks.config.setRules.mockReset(); });

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
    expect(screen.getByDisplayValue('c.test')).toBe(third);
    expect(screen.getByRole('button', { name: 'Move rule 2 up' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Move rule 3 up' })).toBeNull();
  });

  it('retains the remaining input and associates a validation error with its new index', async () => {
    mocks.config.draft.rules = [rule('a.test'), { ...rule('b.test'), max_ttl_secs: 'bad' }];
    const view = render(<CacheRules language="en" />);
    mocks.config.setRules.mockImplementation((rules: CacheRuleDraft[]) => { mocks.config.draft.rules = rules; view.rerender(<CacheRules language="en" />); });
    const ttl = screen.getByDisplayValue('bad');
    const originalId = ttl.id;
    fireEvent.click(screen.getAllByRole('button', { name: 'Remove rule' })[0]);
    mocks.config.error = new ModelError('settings.validation.range', 'cache.rules.0.max_ttl_secs', { min: 0, max: 60 });
    view.rerender(<CacheRules language="en" />);

    expect(screen.getByDisplayValue('bad')).toBe(ttl);
    expect(ttl.id).toBe(originalId);
    expect(document.querySelectorAll(`[id="${originalId}"]`)).toHaveLength(1);
    await waitFor(() => {
      expect(screen.getByDisplayValue('a.test')).toBeTruthy();
      expect(document.activeElement).toBe(ttl);
    });
    expect(ttl.getAttribute('aria-invalid')).toBe('true');
    const description = document.getElementById(ttl.getAttribute('aria-describedby')!);
    expect(description?.textContent).toContain('Enter a whole number from 0 to 60.');
  });
});

// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { CacheInspect } from './SettingsPage';

const mocks = vi.hoisted(() => ({ request: vi.fn(), confirm: vi.fn(async () => true) }));
vi.mock('../../session/context', () => ({ useSession: () => ({ api: { request: mocks.request } }) }));
vi.mock('../../components/ConfirmProvider', () => ({ useConfirm: () => mocks.confirm }));
afterEach(() => { cleanup(); mocks.request.mockReset(); });

const variant = (scope: string, state: string) => ({ name: 'example.test.', qtype: 'A', scope, state, dnssec_ok: false, edns: true, checking_disabled: false, recursion_desired: true, hits: 3 });

describe('cache inspection', () => {
  it('shows candidate flag differences against the submitted inspection, preserving the server explanation', async () => {
    mocks.request.mockResolvedValue({ epoch: 1,
      explanation: { state: 'miss', reason: 'no_matching_retained_entry', scope: null },
      inspection: { variants: [{ ...variant('exact_ecs:192.0.2.0/24', 'fresh'), edns: false }], truncated: false } });
    render(<CacheInspect language="en" dirty={false} />);
    fireEvent.change(screen.getByLabelText('Domain'), { target: { value: 'example.test' } });
    fireEvent.click(screen.getByRole('button', { name: 'Inspect' }));
    expect(await screen.findByText('Different request flags: EDNS')).toBeTruthy();
    expect(screen.getByText('EDNS=0 · DO=0 · CD=0 · RD=1')).toBeTruthy();
    fireEvent.click(screen.getByLabelText('EDNS'));
    expect(screen.getByText('EDNS=1 · DO=0 · CD=0 · RD=1')).toBeTruthy();
    expect(screen.getByText('Different request flags: EDNS')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Inspect' }));
    await waitFor(() => expect(screen.queryByText('Different request flags: EDNS')).toBeNull());
    expect(screen.getByText('Miss: this query must ask an upstream.')).toBeTruthy();
    expect(mocks.request).toHaveBeenLastCalledWith('cache/inspect', 'POST', expect.objectContaining({ edns: false }));
  });
  it('explains a cached DNSSEC failure without suggesting another upstream request', async () => {
    mocks.request.mockResolvedValue({ epoch: 1,
      explanation: { state: 'failure', reason: 'cached_dnssec_failure', scope: 'no_ecs' },
      inspection: { variants: [], truncated: false } });
    render(<CacheInspect language="en" dirty={false} />);
    fireEvent.change(screen.getByLabelText('Domain'), { target: { value: 'bogus.test' } });
    fireEvent.click(screen.getByRole('button', { name: 'Inspect' }));
    expect(await screen.findByText('Cached failure: returns SERVFAIL without asking an upstream.')).toBeTruthy();
    expect(screen.getByText(/A cached DNSSEC validation failure prevents stale fallback/)).toBeTruthy();
  });
  it('lists entries in plain language and clears one with its own scope', async () => {
    let epoch = 4;
    mocks.request.mockImplementation(async (path: string) => {
      if (path === 'cache/inspect') return { epoch, explanation: { state: 'fresh', reason: 'matching_fresh_scope', scope: 'no_ecs' },
        inspection: { variants: epoch === 4 ? [variant('no_ecs', 'fresh'), variant('exact_ecs:192.0.2.0/24', 'stale')] : [variant('no_ecs', 'fresh')], truncated: false } };
      if (path === 'cache/invalidate') { epoch += 1; return { removed: 1, epoch }; }
      throw new Error(path);
    });
    render(<CacheInspect language="en" dirty={false} />);
    expect(screen.getByLabelText('Record type')).toHaveProperty('required', true);
    expect(screen.getByText('More options').closest('details')?.open).toBe(false);
    fireEvent.change(screen.getByLabelText('Domain'), { target: { value: 'example.test' } });
    fireEvent.click(screen.getByRole('button', { name: 'Inspect' }));
    const stale = await screen.findByText('Only for requests from exactly 192.0.2.0/24 · Stale');
    expect(screen.getByText('No ECS; shared by all clients · Fresh')).toBeTruthy();
    expect(screen.queryByLabelText('ECS network')).toBeNull();
    expect(document.querySelector('pre')).toBeNull();
    fireEvent.click(within(stale.closest('li')!).getByRole('button', { name: 'Clear' }));
    await waitFor(() => expect(mocks.request).toHaveBeenCalledWith('cache/invalidate', 'POST', { name: 'example.test.', qtype: 'A', scope: 'exact_ecs:192.0.2.0/24', epoch: 4 }));
    await waitFor(() => expect(screen.queryByText(/192.0.2.0\/24 · Stale/)).toBeNull());
  });
});

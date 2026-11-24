// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { ModelError } from '../model';
import { ConfigErrorNotice, fieldErrorTarget, formatFieldPath } from './fieldErrors';

afterEach(cleanup);

describe('configuration field errors', () => {
  it('identifies the current index and field for cache rules', () => {
    expect(fieldErrorTarget('cache.rules.0.max_ttl_secs')).toMatchObject({ page: 'cache', index: 0, kind: 'rule', labelKey: 'views.positiveTtl' });
  });

  it('targets the matching listener endpoint input', () => {
    expect(fieldErrorTarget('dot.listen.port')).toMatchObject({ id: 'setting-dot-listen-port' });
    expect(fieldErrorTarget('dot.listen.address')).toMatchObject({ id: 'setting-dot-listen-address' });
  });

  it('maps subscription IPv4/IPv6 fields and formats readable paths', () => {
    expect(fieldErrorTarget('filter_subscriptions.sources.0.block_ipv4')).toMatchObject({
      page: 'filters', id: 'setting-filter_subscriptions-sources-0-block_ipv4', kind: 'source', index: 0,
    });
    expect(formatFieldPath('upstreams.max_parallel', 'en')).toBe('Parallel upstreams');
    expect(formatFieldPath('filter_subscriptions.sources.1.block_ipv6', 'zh-CN')).toBe('订阅源 2 · IPv6 地址');
  });

  it('names the field and its page and links back from another page', () => {
    const error = new ModelError('settings.validation.range', 'recursion.max_depth', { min: 4, max: 64 });
    render(<MemoryRouter initialEntries={['/storage']}><ConfigErrorNotice error={error} language="en" /></MemoryRouter>);

    const notice = screen.getByRole('alert');
    expect(notice.textContent).toContain('Upstreams & listeners');
    expect(notice.textContent).toContain('Maximum depth');
    expect(notice.textContent).toContain('Enter a whole number from 4 to 64.');
    expect(screen.getByRole('link').getAttribute('href')).toBe('/dns');
  });
});

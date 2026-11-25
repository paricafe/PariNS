// @vitest-environment jsdom
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ExpandRow } from './ExpandRow';

const groupedRadiusRules = readFileSync(resolve(import.meta.dirname, '../style.css'), 'utf8')
  .split('\n')
  .filter((line) => line.includes('.grouped >') || line.includes('.row-disclosure'))
  .join('\n');

afterEach(cleanup);

describe('ExpandRow', () => {
  it('rounds bottom corners when collapsed both directly inside .grouped and nested inside .settings-expand', () => {
    render(<>
      <style>{groupedRadiusRules}</style>
      <div className="grouped">
        <div className="row">Top</div>
        <ExpandRow label="Direct more"><div className="row">Hidden direct</div></ExpandRow>
      </div>
      <div className="grouped">
        <div className="settings-expand" data-open="true"><div>
          <div className="row">Main</div>
          <ExpandRow label="Nested more"><div className="row">Hidden nested</div></ExpandRow>
        </div></div>
      </div>
    </>);

    for (const name of ['Direct more', 'Nested more']) {
      const toggle = screen.getByRole('button', { name });
      const region = document.getElementById(toggle.getAttribute('aria-controls')!)!;
      expect(toggle.getAttribute('aria-expanded')).toBe('false');
      expect(region.hasAttribute('inert')).toBe(true);
      expect(getComputedStyle(toggle).borderBottomLeftRadius).toBe('calc(var(--radius) - 1px)');
      expect(getComputedStyle(toggle).borderBottomRightRadius).toBe('calc(var(--radius) - 1px)');

      fireEvent.click(toggle);
      expect(toggle.getAttribute('aria-expanded')).toBe('true');
      expect(region.hasAttribute('inert')).toBe(false);
      expect(getComputedStyle(toggle).borderBottomLeftRadius).toBe('0');
      expect(getComputedStyle(toggle).borderBottomRightRadius).toBe('0');
    }
  });
});

import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const expected = ['assets/app.css', 'assets/app.js', 'index.html', 'theme-init.js'];
const actual = [
  ...readdirSync('dist/assets').map((name) => `assets/${name}`),
  ...readdirSync('dist').filter((name) => name !== 'assets'),
].sort();
if (JSON.stringify(actual) !== JSON.stringify(expected)) {
  throw new Error(`Unexpected frontend assets: ${actual.join(', ')}`);
}
const html = readFileSync(join('dist', 'index.html'), 'utf8');
const css = readFileSync(join('dist', 'assets', 'app.css'), 'utf8');
// Any Tailwind dark: utility must follow the console's .dark class, never the
// media query, so the theme selector stays authoritative.
for (const [rule] of css.matchAll(/\.dark\\:[^{]+\{/g)) {
  if (!rule.includes(':where(.dark,.dark *)')) {
    throw new Error(`Dark utility must follow the console's .dark theme class: ${rule.slice(0, 80)}`);
  }
}
for (const resource of ['/theme-init.js', '/assets/app.js', '/assets/app.css']) {
  if (!html.includes(resource)) throw new Error(`Missing embedded resource ${resource}`);
}
if (/\b(?:https?:)?\/\//i.test(html)) throw new Error('Remote resources are not allowed in the embedded page');
if (/<script(?![^>]+src=)/i.test(html)) throw new Error('Inline script is not allowed');

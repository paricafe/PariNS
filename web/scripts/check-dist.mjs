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
for (const resource of ['/theme-init.js', '/assets/app.js', '/assets/app.css']) {
  if (!html.includes(resource)) throw new Error(`Missing embedded resource ${resource}`);
}
if (/\b(?:https?:)?\/\//i.test(html)) throw new Error('Remote resources are not allowed in the embedded page');
if (/<script(?![^>]+src=)/i.test(html)) throw new Error('Inline script is not allowed');

import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { brotliDecompressSync, gunzipSync } from 'node:zlib';

const expected = [
  'assets/app.css', 'assets/app.css.br', 'assets/app.css.gz',
  'assets/app.js', 'assets/app.js.br', 'assets/app.js.gz',
  'favicon.svg', 'index.html', 'theme-init.js',
];
const actual = [
  ...readdirSync('dist/assets').map((name) => `assets/${name}`),
  ...readdirSync('dist').filter((name) => name !== 'assets'),
].sort();
if (JSON.stringify(actual) !== JSON.stringify(expected)) {
  throw new Error(`Unexpected frontend assets: ${actual.join(', ')}`);
}
for (const file of ['assets/app.js', 'assets/app.css']) {
  const content = readFileSync(join('dist', file));
  for (const [suffix, decompress] of [['.br', brotliDecompressSync], ['.gz', gunzipSync]]) {
    if (!decompress(readFileSync(join('dist', file + suffix))).equals(content)) {
      throw new Error(`${file}${suffix} does not match ${file}`);
    }
  }
}
const html = readFileSync(join('dist', 'index.html'), 'utf8');
for (const resource of ['/favicon.svg', '/theme-init.js', '/assets/app.js', '/assets/app.css']) {
  const digest = createHash('sha256').update(readFileSync(join('dist', resource))).digest('hex');
  if (!html.includes(`"${resource}?v=${digest.slice(0, 16)}"`)) {
    throw new Error(`Missing or stale version of embedded resource ${resource}`);
  }
}
if (/\b(?:https?:)?\/\//i.test(html)) throw new Error('Remote resources are not allowed in the embedded page');
if (/<script(?![^>]+src=)/i.test(html)) throw new Error('Inline script is not allowed');

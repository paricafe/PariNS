// Precompresses the large console files and versions every resource address in
// index.html, so the management server can send them encoded and cacheable.
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';

const compressed = ['assets/app.js', 'assets/app.css'];
const versioned = ['/favicon.svg', '/theme-init.js', '/assets/app.js', '/assets/app.css'];

for (const file of compressed) {
  const content = readFileSync(join('dist', file));
  writeFileSync(join('dist', `${file}.br`), brotliCompressSync(content, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: 11,
      [constants.BROTLI_PARAM_SIZE_HINT]: content.length,
    },
  }));
  writeFileSync(join('dist', `${file}.gz`), gzipSync(content, { level: 9 }));
}

let html = readFileSync(join('dist', 'index.html'), 'utf8');
for (const resource of versioned) {
  const reference = `"${resource}"`;
  if (!html.includes(reference)) throw new Error(`Missing embedded resource ${resource}`);
  const digest = createHash('sha256').update(readFileSync(join('dist', resource))).digest('hex');
  html = html.replace(reference, `"${resource}?v=${digest.slice(0, 16)}"`);
}
writeFileSync(join('dist', 'index.html'), html);

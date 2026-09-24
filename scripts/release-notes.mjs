import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

// CHANGELOG.md keeps history; a GitHub Release gets only its own section.
export function releaseNotes(changelog, version) {
  if (!/^v\d+\.\d+\.\d+$/.test(version)) throw new Error('Expected version vX.Y.Z');
  const sections = [...changelog.matchAll(/^## (.+)\r?$/gm)];
  const matching = sections.filter(section => section[1].trim().split(/\s+/)[0] === version);
  if (matching.length !== 1) throw new Error(`Expected exactly one changelog section for ${version}`);
  const section = matching[0];
  const next = sections[sections.indexOf(section) + 1];
  const notes = changelog.slice(section.index + section[0].length, next?.index).trim();
  if (!notes) throw new Error(`Empty changelog section for ${version}`);
  return `${notes}\n`;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    process.stdout.write(releaseNotes(readFileSync(new URL('../CHANGELOG.md', import.meta.url), 'utf8'), process.argv[2]));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}

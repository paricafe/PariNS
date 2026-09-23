export interface DiffLine { kind: 'same' | 'add' | 'remove'; text: string }

export function lineDiff(before: string, after: string): DiffLine[] {
  const oldLines = before.split('\n');
  const newLines = after.split('\n');
  const rows = oldLines.length + 1;
  const columns = newLines.length + 1;
  if (rows * columns > 400_000) {
    let prefix = 0;
    while (prefix < oldLines.length && prefix < newLines.length && oldLines[prefix] === newLines[prefix]) prefix += 1;
    let suffix = 0;
    while (suffix < oldLines.length - prefix && suffix < newLines.length - prefix && oldLines[oldLines.length - suffix - 1] === newLines[newLines.length - suffix - 1]) suffix += 1;
    return [...oldLines.slice(0, prefix).map((text) => ({ kind: 'same' as const, text })),
      ...oldLines.slice(prefix, oldLines.length - suffix).map((text) => ({ kind: 'remove' as const, text })),
      ...newLines.slice(prefix, newLines.length - suffix).map((text) => ({ kind: 'add' as const, text })),
      ...oldLines.slice(oldLines.length - suffix).map((text) => ({ kind: 'same' as const, text }))];
  }
  const scores = new Uint32Array(rows * columns);
  const at = (oldIndex: number, newIndex: number) => oldIndex * columns + newIndex;
  for (let oldIndex = oldLines.length - 1; oldIndex >= 0; oldIndex--) {
    for (let newIndex = newLines.length - 1; newIndex >= 0; newIndex--) {
      scores[at(oldIndex, newIndex)] = oldLines[oldIndex] === newLines[newIndex]
        ? 1 + scores[at(oldIndex + 1, newIndex + 1)]
        : Math.max(scores[at(oldIndex + 1, newIndex)], scores[at(oldIndex, newIndex + 1)]);
    }
  }
  const output: DiffLine[] = [];
  let oldIndex = 0; let newIndex = 0;
  while (oldIndex < oldLines.length || newIndex < newLines.length) {
    if (oldIndex < oldLines.length && newIndex < newLines.length && oldLines[oldIndex] === newLines[newIndex]) {
      output.push({ kind: 'same', text: oldLines[oldIndex++] }); newIndex += 1;
    } else if (newIndex < newLines.length && (oldIndex === oldLines.length || scores[at(oldIndex, newIndex + 1)] > scores[at(oldIndex + 1, newIndex)])) {
      output.push({ kind: 'add', text: newLines[newIndex++] });
    } else {
      output.push({ kind: 'remove', text: oldLines[oldIndex++] });
    }
  }
  return output;
}

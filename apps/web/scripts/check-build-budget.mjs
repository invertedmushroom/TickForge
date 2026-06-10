import { readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const distAssets = fileURLToPath(new URL('../dist/assets/', import.meta.url));
const budgetsKiB = [
  { prefix: 'index-', maxKiB: 200 },
  { prefix: 'scene-', maxKiB: 150 },
  { prefix: 'vendor-', maxKiB: 300 },
  { prefix: 'three-vendor-', maxKiB: 650 },
  { prefix: 'rapier-vendor-', maxKiB: 2_500 },
];

const files = await readdir(distAssets);
const violations = [];

for (const budget of budgetsKiB) {
  const file = files.find((candidate) => candidate.startsWith(budget.prefix) && candidate.endsWith('.js'));
  if (!file) {
    violations.push(`${budget.prefix}*.js was not emitted`);
    continue;
  }

  const sizeKiB = (await stat(join(distAssets, file))).size / 1024;
  if (sizeKiB > budget.maxKiB) {
    violations.push(`${file} is ${sizeKiB.toFixed(1)} KiB, budget is ${budget.maxKiB} KiB`);
  }
}

if (violations.length > 0) {
  console.error('Build budget check failed:\n');
  for (const violation of violations) {
    console.error(`- ${violation}`);
  }
  process.exit(1);
}

console.log('Build budget check passed.');

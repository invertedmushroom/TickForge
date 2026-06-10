import { readdir, readFile, realpath } from 'node:fs/promises';
import { dirname, extname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = fileURLToPath(new URL('../', import.meta.url));
const srcRoot = fileURLToPath(new URL('../src/', import.meta.url));
const expectedContractSpec = 'file:../../target/web-contract';
const expectedContractRoot = fileURLToPath(new URL('../../../target/web-contract/', import.meta.url));
const expectedAbilitiesPath = join(expectedContractRoot, 'abilities.json');
const expectedPhysicsPredictionPath = join(expectedContractRoot, 'physics-prediction.json');
const violations = [];

await verifyContractPackage();

const policyUrl = await import.meta.resolve('@dive/client-contract/browser-policy.json');
const policy = JSON.parse(await readFile(fileURLToPath(policyUrl), 'utf8'));
const sourceExtensions = new Set(['.ts', '.tsx', '.js', '.jsx']);

const forbiddenTables = policy.forbidden_tables;
const forbiddenReducers = new Set(
  policy.forbidden_reducers.flatMap((name) => [name, toSnakeCase(name)]),
);

for (const file of await collectFiles(srcRoot)) {
  const text = await readFile(file, 'utf8');
  const normalized = file.replaceAll('\\', '/');

  for (const table of forbiddenTables) {
    const sqlPattern = new RegExp(`\\bSELECT\\s+\\*\\s+FROM\\s+${table}\\b`, 'i');
    const accessorPattern = new RegExp(`\\b(db|tables)\\.${toCamelCase(table)}\\b`);
    if (sqlPattern.test(text) || accessorPattern.test(text)) {
      violations.push(`${normalized}: forbidden browser table access: ${table}`);
    }
  }

  for (const reducer of forbiddenReducers) {
    const reducerPattern = new RegExp(`\\b${reducer}\\b`);
    if (reducerPattern.test(text)) {
      violations.push(`${normalized}: forbidden browser reducer use: ${reducer}`);
    }
  }

  if (/\breducers\.debug[A-Z][A-Za-z0-9_]*\b|\bdebug_[a-z0-9_]+_reducer\b/.test(text)) {
    violations.push(`${normalized}: debug reducers are forbidden in browser code`);
  }

  if (/subscribeToAllTables\s*\(/.test(text)) {
    violations.push(`${normalized}: subscribeToAllTables() is forbidden in browser code`);
  }
}

if (violations.length > 0) {
  console.error('Forbidden subscription/reducer usage found:\n');
  for (const violation of violations) {
    console.error(`- ${violation}`);
  }
  process.exit(1);
}

console.log('Browser contract and subscription policy checks passed.');

async function verifyContractPackage() {
  const packageJson = JSON.parse(await readFile(join(repoRoot, 'package.json'), 'utf8'));
  const actualSpec = packageJson.dependencies?.['@dive/client-contract'];
  if (actualSpec !== expectedContractSpec) {
    violations.push(`package.json: @dive/client-contract must be ${expectedContractSpec}, got ${actualSpec ?? 'missing'}`);
  }

  const lockJson = JSON.parse(await readFile(join(repoRoot, 'package-lock.json'), 'utf8'));
  const lockSpec = lockJson.packages?.['']?.dependencies?.['@dive/client-contract'];
  const lockResolved = lockJson.packages?.['node_modules/@dive/client-contract']?.resolved;
  if (lockSpec !== expectedContractSpec) {
    violations.push(
      `package-lock.json: root @dive/client-contract must be ${expectedContractSpec}, got ${lockSpec ?? 'missing'}`,
    );
  }
  if (lockResolved !== '../../target/web-contract') {
    violations.push(
      `package-lock.json: node_modules/@dive/client-contract must resolve to ../../target/web-contract, got ${lockResolved ?? 'missing'}`,
    );
  }

  const abilityUrl = await import.meta.resolve('@dive/client-contract/abilities.json');
  const resolvedAbilitiesPath = fileURLToPath(abilityUrl);
  const [resolvedRealPath, expectedRealPath] = await Promise.all([
    realpath(resolvedAbilitiesPath),
    realpath(expectedAbilitiesPath),
  ]);
  if (resolvedRealPath !== expectedRealPath) {
    violations.push(
      `@dive/client-contract/abilities.json resolves to ${relative(repoRoot, resolvedRealPath)}, expected ${relative(
        repoRoot,
        expectedRealPath,
      )}`,
    );
  }

  const contractPackage = JSON.parse(await readFile(join(dirname(expectedRealPath), 'package.json'), 'utf8'));
  if (contractPackage.exports?.['./abilities.json'] !== './abilities.json') {
    violations.push('target/web-contract/package.json: missing export for ./abilities.json');
  }
  if (contractPackage.exports?.['./physics-prediction.json'] !== './physics-prediction.json') {
    violations.push('target/web-contract/package.json: missing export for ./physics-prediction.json');
  }

  const abilities = JSON.parse(await readFile(expectedRealPath, 'utf8'));
  if (abilities.schema_version !== 1 || !Array.isArray(abilities.abilities) || abilities.abilities.length === 0) {
    violations.push('target/web-contract/abilities.json: missing schema_version=1 ability metadata');
  }

  const [physicsPrediction, contractManifest] = await Promise.all([
    readJson(expectedPhysicsPredictionPath),
    readJson(join(dirname(expectedRealPath), 'contract.json')),
  ]);
  if (physicsPrediction.schema_version !== 1 || physicsPrediction.hash !== contractManifest.physics_hash) {
    violations.push('target/web-contract/physics-prediction.json: missing schema_version=1 or matching physics hash');
  }
}

async function readJson(path) {
  return JSON.parse(await readFile(path, 'utf8'));
}

async function collectFiles(root) {
  const files = [];
  await walk(root, files);
  return files;
}

async function walk(dir, files) {
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const fullPath = join(dir, entry.name);
    if (entry.isDirectory()) {
      await walk(fullPath, files);
    } else if (entry.isFile() && sourceExtensions.has(extname(entry.name))) {
      files.push(fullPath);
    }
  }
}

function toCamelCase(value) {
  return value.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase());
}

function toSnakeCase(value) {
  return value.replace(/[A-Z]/g, (letter) => `_${letter.toLowerCase()}`);
}

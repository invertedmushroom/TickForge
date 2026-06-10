import './styles.css';
import { ALWAYS_ON_SUBSCRIPTIONS, validateStartupContract } from './contract';
import { createStdbClient } from './stdb/connection';
import { createMapAssetProvider } from './world/mapAssets';
import { mountAppShell, initializeAppRuntime } from './app';

const app = document.querySelector<HTMLDivElement>('#app');

if (!app) {
  throw new Error('missing #app root');
}

const shell = mountAppShell(app, ALWAYS_ON_SUBSCRIPTIONS.length);

try {
  const contract = validateStartupContract();
  const mapAssets = createMapAssetProvider({ expectedContentHash: contract.contentHash });
  const startupHandles = mapAssets.loadStartupMap();
  // Gate gameplay startup on physics only; visual loads independently downstream.
  const startupPhysics = await startupHandles.physics;
  const stdbClient = createStdbClient();

  const runtime = initializeAppRuntime({
    shell,
    contract: {
      contentHash: contract.contentHash,
      packageName: contract.packageName,
      version: contract.version,
      schemaHash: contract.schemaHash,
      physicsHash: contract.physicsHash,
      layers: contract.layers,
      terrainSets: contract.terrainSets,
      abilityCount: contract.abilityCount,
    },
    startupHandles,
    startupPhysics,
    mapAssets,
    stdbClient,
  });

  if (import.meta.env.VITE_STDB_AUTOCONNECT !== 'false') {
    stdbClient.connect();
  }

  await runtime.sceneManager.initialize(startupHandles, startupPhysics);
  if (import.meta.hot) {
    import.meta.hot.dispose(() => {
      runtime.cleanup();
      stdbClient.disconnect();
    });
  }
  shell.startupStatus.textContent = `${shell.startupStatus.textContent} · Rapier ready`;
} catch (error) {
  const message = error instanceof Error ? error.message : String(error);
  shell.startupStatus.textContent = `startup failed: ${message}`;
  shell.startupStatus.classList.add('diagnostics__line--error');
  throw error;
}

import { type StdbClient } from './stdb/connection';
import { createGameplayHud, type GameplayHudHandle } from './ui/gameplayHud';
import { createLootHud, type LootHudHandle } from './ui/lootHud';
import { createDiagnosticsPanel, type DiagnosticsPanel } from './ui/diagnostics';
import { createMapLoadingHud, type MapLoadingHudHandle } from './ui/mapLoadingHud';
import { mountUiRuntimeShell, type UiRuntimeShell } from './ui/runtime';
import { mountSettingsEntry, type SettingsEntryHandle } from './ui/settingsEntry';
import { SceneManager } from './sceneManager';
import { mapBundleSummary } from './world/content';
import type {
  LoadedPhysicsMap,
  MapAssetHandles,
  MapAssetProvider,
} from './world/mapAssets';
import { shortHash } from './contract';
import { RESEND_HZ } from './net/inputQueue';

export type AppShell = UiRuntimeShell;

export type AppRuntime = {
  stdbClient: StdbClient;
  gameplayHud: GameplayHudHandle;
  lootHud: LootHudHandle;
  diagnostics: DiagnosticsPanel;
  mapLoadingHud: MapLoadingHudHandle;
  sceneManager: SceneManager;
  settingsEntry: SettingsEntryHandle;
  cleanup: () => void;
};

export function mountAppShell(container: HTMLElement, subscriptionCount: number): AppShell {
  const shell = mountUiRuntimeShell(container, { subscriptionCount });
  window.__DIVE_WEB_UI_RUNTIME__ = shell.uiRuntime;
  return shell;
}

export function initializeAppRuntime(options: {
  shell: AppShell;
  contract: {
    contentHash: string;
    packageName: string;
    version: string;
    schemaHash: string;
    physicsHash: string;
    layers: number;
    terrainSets: number;
    abilityCount: number;
  };
  startupHandles: MapAssetHandles;
  startupPhysics: LoadedPhysicsMap;
  mapAssets: MapAssetProvider;
  stdbClient: StdbClient;
}): AppRuntime {
  const { shell, contract, startupHandles, startupPhysics, mapAssets, stdbClient } = options;

  const gameplayHud = createGameplayHud(shell.gameplayHudRoot, {
    onRespawn: () => void stdbClient.respawnPlayer(),
  });
  const lootHud = createLootHud(shell.gameplayHudRoot, {
    stdbClient,
    runtime: shell.uiRuntime,
  });

  const diagnostics = createDiagnosticsPanel({
    connectionStatus: shell.connectionStatus,
    inputStatus: shell.inputStatus,
    predictionStatus: shell.predictionStatus,
    remoteStatus: shell.remoteStatus,
    mapStatus: shell.mapStatus,
    gameplayHud,
  });

  shell.contractStatus.textContent = `${contract.packageName} ${contract.version} schema ${shortHash(contract.schemaHash)} · abilities ${contract.abilityCount}`;
  shell.startupStatus.textContent = `content ${shortHash(contract.contentHash)} · physics ${shortHash(contract.physicsHash)} · layers ${contract.layers} · terrain ${contract.terrainSets}`;
  diagnostics.setMapStatus(`map: ${mapBundleSummary(startupPhysics.bundle)} · physics ${startupPhysics.cache.source}`);

  const mapLoadingHud = createMapLoadingHud(shell.mapLoadingRoot, {
    onRetry: () => {
      sceneManager.maybeSwitchMapBundle(stdbClient.snapshot());
    },
    onClearCache: () => {
      void mapAssets.clearCache().then(() => {
        sceneManager.maybeSwitchMapBundle(stdbClient.snapshot());
      });
    },
  });

  // Surface visual channel resolution without blocking gameplay startup.
  void startupHandles.visual
    .then((visual) => {
      if (visual) {
        diagnostics.setMapStatus(
          `map: ${mapBundleSummary(startupPhysics.bundle)} · physics ${startupPhysics.cache.source} · visual ${visual.cache.source}`,
        );
      }
    })
    .catch((error: unknown) => {
      const message = error instanceof Error ? error.message : String(error);
      diagnostics.setMapStatus(
        `map: ${mapBundleSummary(startupPhysics.bundle)} · physics ${startupPhysics.cache.source} · visual failed: ${message}`,
        true,
      );
    });

  const sceneManager = new SceneManager({
    sceneRoot: shell.sceneRoot,
    actionBarRoot: shell.actionBarRoot,
    touchControlsRoot: shell.touchControlsRoot,
    inputMode: shell.uiRuntime.inputMode,
    subscribeInputMode: shell.uiRuntime.subscribeInputMode,
    touchControlsPreferences: () => shell.uiRuntime.preferences().touchControls,
    subscribeTouchControlsPreferences: (listener) =>
      shell.uiRuntime.subscribePreferences((preferences) => listener(preferences.touchControls)),
    stdbClient,
    diagnostics,
    mapLoadingHud,
    contentHash: contract.contentHash,
    mapAssets,
  });

  let latestSnapshot = stdbClient.snapshot();

  const unsubscribeDiagnostics = stdbClient.subscribe((snapshot) => {
    latestSnapshot = snapshot;
    sceneManager.maybeSwitchMapBundle(snapshot);
    diagnostics.render(snapshot, window.__DIVE_WEB_SCENE_STATS__);
  });

  const diagnosticsTimer = window.setInterval(() => diagnostics.render(latestSnapshot, window.__DIVE_WEB_SCENE_STATS__), 250);
  const resendTimer = window.setInterval(() => {
    void stdbClient.resendPendingIntents();
  }, 1000 / RESEND_HZ);

  const diagnosticsElement = shell.shellRoot.querySelector<HTMLElement>('.diagnostics');
  const settingsEntry = mountSettingsEntry({
    runtime: shell.uiRuntime,
    buttonHost: shell.shellRoot,
    diagnosticsElement,
  });

  return {
    stdbClient,
    gameplayHud,
    lootHud,
    diagnostics,
    mapLoadingHud,
    sceneManager,
    settingsEntry,
    cleanup: () => {
      window.clearInterval(diagnosticsTimer);
      window.clearInterval(resendTimer);
      unsubscribeDiagnostics();
      settingsEntry.destroy();
      mapLoadingHud.destroy();
      lootHud.destroy();
      gameplayHud.destroy();
      sceneManager.destroy();
      if (window.__DIVE_WEB_UI_RUNTIME__ === shell.uiRuntime) {
        delete window.__DIVE_WEB_UI_RUNTIME__;
      }
      shell.uiRuntime.destroy();
    },
  };
}

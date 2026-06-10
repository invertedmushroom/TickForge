import type { SceneHandle } from './render/scene';
import { mapBundleSummary, type LoadedMapBundle } from './world/content';
import {
  createMapAssetProvider,
  type LoadedPhysicsMap,
  type MapAssetCacheInfo,
  type MapAssetHandles,
  type MapAssetProvider,
} from './world/mapAssets';
import type { StdbClient, StdbSnapshot } from './stdb/connection';
import type { DiagnosticsPanel } from './ui/diagnostics';
import type { MapLoadingHudHandle, MapLoadingHudState } from './ui/mapLoadingHud';
import type { InputFocusMode } from './input/focusGate';
import type { TouchControlPreferences } from './input/touchPreferences';

export type SceneManagerOptions = {
  sceneRoot: HTMLElement;
  actionBarRoot?: HTMLElement;
  touchControlsRoot?: HTMLElement;
  inputMode?: () => InputFocusMode;
  subscribeInputMode?: (listener: (mode: InputFocusMode) => void) => () => void;
  touchControlsPreferences?: () => TouchControlPreferences;
  subscribeTouchControlsPreferences?: (listener: (preferences: TouchControlPreferences) => void) => () => void;
  stdbClient: StdbClient;
  diagnostics: DiagnosticsPanel;
  mapLoadingHud?: MapLoadingHudHandle;
  contentHash: string;
  mapAssets?: MapAssetProvider;
};

export class SceneManager {
  private readonly sceneRoot: HTMLElement;
  private readonly actionBarRoot?: HTMLElement;
  private readonly touchControlsRoot?: HTMLElement;
  private readonly inputMode?: () => InputFocusMode;
  private readonly subscribeInputMode?: (listener: (mode: InputFocusMode) => void) => () => void;
  private readonly touchControlsPreferences?: () => TouchControlPreferences;
  private readonly subscribeTouchControlsPreferences?: (listener: (preferences: TouchControlPreferences) => void) => () => void;
  private readonly stdbClient: StdbClient;
  private readonly diagnostics: DiagnosticsPanel;
  private readonly mapLoadingHud?: MapLoadingHudHandle;
  private readonly mapAssets: MapAssetProvider;
  private activeBundle?: LoadedMapBundle;
  private activePhysicsCache?: MapAssetCacheInfo;
  private activeVisualCache?: MapAssetCacheInfo;
  private sceneHandle?: SceneHandle;
  private mapTransitionInFlight = false;
  private mapTransitionError?: string;
  private currentMapLoadingState: MapLoadingHudState = { active: false };
  private sceneGeneration = 0;
  private initialized = false;
  private destroyed = false;

  constructor(options: SceneManagerOptions) {
    this.sceneRoot = options.sceneRoot;
    this.actionBarRoot = options.actionBarRoot;
    this.touchControlsRoot = options.touchControlsRoot;
    this.inputMode = options.inputMode;
    this.subscribeInputMode = options.subscribeInputMode;
    this.touchControlsPreferences = options.touchControlsPreferences;
    this.subscribeTouchControlsPreferences = options.subscribeTouchControlsPreferences;
    this.stdbClient = options.stdbClient;
    this.diagnostics = options.diagnostics;
    this.mapLoadingHud = options.mapLoadingHud;
    this.mapAssets =
      options.mapAssets ?? createMapAssetProvider({ expectedContentHash: options.contentHash });
  }

  private updateMapLoadingState(update: Partial<MapLoadingHudState>) {
    this.currentMapLoadingState = { ...this.currentMapLoadingState, ...update };
    this.mapLoadingHud?.setState(this.currentMapLoadingState);
  }

  async initialize(handles: MapAssetHandles, physics: LoadedPhysicsMap): Promise<void> {
    if (this.destroyed) {
      return;
    }
    await this.replaceScene(handles, physics);
    if (!this.destroyed) {
      this.initialized = true;
    }
  }

  maybeSwitchMapBundle(snapshot: StdbSnapshot): void {
    if (this.destroyed || !this.initialized || this.mapTransitionInFlight) {
      return;
    }

    try {
      const nextBundleId = this.mapAssets.resolveRuntimeBundleId(snapshot);
      if (this.activeBundle?.bundleId === nextBundleId) {
        this.mapTransitionError = undefined;
        this.diagnostics.setMapStatus(`map: ${this.activeMapSummary()}`);
        this.updateMapLoadingState({ active: false });
        return;
      }
      this.mapTransitionInFlight = true;
      this.diagnostics.setMapStatus(`map: loading ${nextBundleId}`);
      this.updateMapLoadingState({
        active: true,
        bundleId: nextBundleId,
        physicsStatus: 'loading',
        visualStatus: 'loading',
        errorMessage: undefined,
      });

      const handles = this.mapAssets.loadRuntimeMap(snapshot);
      void handles.physics
        .then((physics) => this.replaceScene(handles, physics))
        .catch((error: unknown) => {
          this.mapTransitionInFlight = false;
          this.mapTransitionError = error instanceof Error ? error.message : String(error);
          this.diagnostics.setMapStatus(`map: transition failed: ${this.mapTransitionError}`, true);
          this.updateMapLoadingState({ physicsStatus: 'error', errorMessage: this.mapTransitionError });
        });
    } catch (error) {
      this.mapTransitionError = error instanceof Error ? error.message : String(error);
      this.diagnostics.setMapStatus(`map: ${this.mapTransitionError}`, true);
      this.updateMapLoadingState({ 
        active: true, 
        physicsStatus: 'error', 
        errorMessage: this.mapTransitionError 
      });
    }
  }

  destroy(): void {
    this.destroyed = true;
    this.sceneGeneration += 1;
    this.mapTransitionInFlight = false;
    this.sceneHandle?.destroy();
    this.sceneHandle = undefined;
    this.activeBundle = undefined;
    this.activePhysicsCache = undefined;
    this.activeVisualCache = undefined;
  }

  private async replaceScene(handles: MapAssetHandles, physics: LoadedPhysicsMap): Promise<void> {
    if (this.destroyed) {
      return;
    }
    const generation = ++this.sceneGeneration;
    this.mapTransitionInFlight = true;
    this.mapTransitionError = undefined;
    this.diagnostics.setMapStatus(
      `map: loading ${mapBundleSummary(physics.bundle)} · physics ${cacheSummary(physics.cache)}`,
    );
    const previousHandle = this.sceneHandle;

    const { mountScene } = await import('./render/scene');
    const nextHandle = await mountScene(this.sceneRoot, this.stdbClient, physics.bundle, {
      inputEnabled: () => !this.mapTransitionInFlight,
      inputMode: this.inputMode,
      subscribeInputMode: this.subscribeInputMode,
      actionBarRoot: this.actionBarRoot,
      touchControlsRoot: this.touchControlsRoot,
      touchControlsPreferences: this.touchControlsPreferences,
      subscribeTouchControlsPreferences: this.subscribeTouchControlsPreferences,
      visual: handles.visual,
    });

    if (this.destroyed || generation !== this.sceneGeneration) {
      nextHandle.destroy();
      return;
    }

    previousHandle?.destroy();
    this.sceneHandle = nextHandle;
    this.activeBundle = physics.bundle;
    this.activePhysicsCache = physics.cache;
    this.activeVisualCache = undefined;
    this.mapTransitionInFlight = false;
    this.diagnostics.setMapStatus(`map: ${this.activeMapSummary()}`);
    this.updateMapLoadingState({
      physicsStatus: 'done',
      physicsSource: physics.cache.source,
    });

    this.trackVisualChannel(handles, generation);
  }

  private trackVisualChannel(handles: MapAssetHandles, generation: number): void {
    void handles.visual
      .then((visual) => {
        if (this.destroyed || generation !== this.sceneGeneration) {
          return;
        }
        if (!visual) {
          this.updateMapLoadingState({ active: false });
          return;
        }
        this.activeVisualCache = visual.cache;
        this.diagnostics.setMapStatus(`map: ${this.activeMapSummary()}`);
        this.updateMapLoadingState({
          visualStatus: 'done',
          visualSource: visual.cache.source,
          active: false, // Visuals done, we can hide the panel
        });
      })
      .catch((error: unknown) => {
        if (this.destroyed || generation !== this.sceneGeneration) {
          return;
        }
        const message = error instanceof Error ? error.message : String(error);
        this.diagnostics.setMapStatus(
          `map: ${this.activeMapSummary()} · visual failed: ${message}`,
          true,
        );
        this.updateMapLoadingState({
          visualStatus: 'error',
          errorMessage: `Visual load failed: ${message}`,
        });
      });
  }

  private activeMapSummary(): string {
    if (!this.activeBundle) {
      return 'none';
    }
    const physics = this.activePhysicsCache ? ` · physics ${cacheSummary(this.activePhysicsCache)}` : '';
    const visual = this.activeVisualCache ? ` · visual ${cacheSummary(this.activeVisualCache)}` : '';
    return `${mapBundleSummary(this.activeBundle)}${physics}${visual}`;
  }
}

function cacheSummary(cache: MapAssetCacheInfo): string {
  switch (cache.status) {
    case 'hit':
      return `${cache.source} cache`;
    case 'stored':
      return `${cache.source} cached`;
    case 'fallback':
      return 'embedded fallback';
    case 'embedded':
      return 'embedded';
    case 'bypassed':
      return `${cache.source} (cache bypassed)`;
    default: {
      const _exhaustive: never = cache.status;
      return _exhaustive;
    }
  }
}

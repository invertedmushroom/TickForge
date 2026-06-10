export type UiCapabilityState = {
  wakeLockSupported: boolean;
  wakeLockRequested: boolean;
  wakeLockActive: boolean;
  wakeLockError?: string;
  fullscreenSupported: boolean;
  fullscreenRequested: boolean;
  fullscreenActive: boolean;
  fullscreenError?: string;
};

export type UiCapabilityManager = {
  state: () => UiCapabilityState;
  setWakeLock: (active: boolean) => Promise<UiCapabilityState>;
  setFullscreen: (active: boolean) => Promise<UiCapabilityState>;
  subscribe: (listener: (state: UiCapabilityState) => void) => () => void;
  destroy: () => void;
};

type WakeLockSentinelLike = EventTarget & {
  released?: boolean;
  release: () => Promise<void>;
};

type WakeLockLike = {
  request: (type: 'screen') => Promise<WakeLockSentinelLike>;
};

export function createUiCapabilityManager(root: HTMLElement): UiCapabilityManager {
  const listeners = new Set<(state: UiCapabilityState) => void>();
  let wakeLockRequested = false;
  let wakeLockSentinel: WakeLockSentinelLike | undefined;
  let wakeLockError: string | undefined;
  let fullscreenRequested = false;
  let fullscreenError: string | undefined;
  let destroyed = false;

  const emit = () => {
    const nextState = currentState();
    applyCapabilityDataset(root, nextState);
    for (const listener of listeners) {
      listener(nextState);
    }
  };

  const onWakeLockRelease = () => {
    wakeLockSentinel?.removeEventListener('release', onWakeLockRelease);
    wakeLockSentinel = undefined;
    emit();
  };

  const requestWakeLock = async () => {
    const wakeLock = wakeLockFromNavigator();
    if (!wakeLock) {
      wakeLockError = 'unsupported';
      emit();
      return;
    }
    if (document.visibilityState === 'hidden') {
      wakeLockError = 'document_hidden';
      emit();
      return;
    }
    try {
      wakeLockError = undefined;
      wakeLockSentinel?.removeEventListener('release', onWakeLockRelease);
      wakeLockSentinel = await wakeLock.request('screen');
      wakeLockSentinel.addEventListener('release', onWakeLockRelease);
    } catch (error) {
      wakeLockSentinel = undefined;
      wakeLockError = errorMessage(error);
    }
    emit();
  };

  const releaseWakeLock = async () => {
    wakeLockError = undefined;
    const sentinel = wakeLockSentinel;
    wakeLockSentinel = undefined;
    sentinel?.removeEventListener('release', onWakeLockRelease);
    try {
      await sentinel?.release();
    } catch (error) {
      wakeLockError = errorMessage(error);
    }
    emit();
  };

  const onVisibilityChange = () => {
    if (!wakeLockRequested || destroyed) {
      return;
    }
    if (document.visibilityState === 'visible' && !wakeLockSentinel) {
      void requestWakeLock();
    }
  };

  const onFullscreenChange = () => {
    fullscreenError = undefined;
    if (!document.fullscreenElement) {
      fullscreenRequested = false;
    }
    emit();
  };

  const currentState = (): UiCapabilityState => ({
    wakeLockSupported: wakeLockFromNavigator() !== undefined,
    wakeLockRequested,
    wakeLockActive: wakeLockSentinel !== undefined && wakeLockSentinel.released !== true,
    wakeLockError,
    fullscreenSupported: fullscreenSupported(root),
    fullscreenRequested,
    fullscreenActive: document.fullscreenElement === root,
    fullscreenError,
  });

  document.addEventListener('visibilitychange', onVisibilityChange);
  document.addEventListener('fullscreenchange', onFullscreenChange);
  applyCapabilityDataset(root, currentState());

  return {
    state: () => ({ ...currentState() }),
    async setWakeLock(active) {
      wakeLockRequested = active;
      if (active) {
        await requestWakeLock();
      } else {
        await releaseWakeLock();
      }
      return { ...currentState() };
    },
    async setFullscreen(active) {
      fullscreenRequested = active;
      fullscreenError = undefined;
      if (active) {
        if (!fullscreenSupported(root)) {
          fullscreenError = 'unsupported';
          emit();
          return { ...currentState() };
        }
        try {
          await root.requestFullscreen();
        } catch (error) {
          fullscreenRequested = false;
          fullscreenError = errorMessage(error);
        }
      } else if (document.fullscreenElement === root) {
        try {
          await document.exitFullscreen();
        } catch (error) {
          fullscreenError = errorMessage(error);
        }
      }
      emit();
      return { ...currentState() };
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    destroy() {
      if (destroyed) {
        return;
      }
      destroyed = true;
      document.removeEventListener('visibilitychange', onVisibilityChange);
      document.removeEventListener('fullscreenchange', onFullscreenChange);
      listeners.clear();
      void releaseWakeLock();
      if (document.fullscreenElement === root) {
        void document.exitFullscreen().catch(() => undefined);
      }
    },
  };
}

function wakeLockFromNavigator(): WakeLockLike | undefined {
  const maybeNavigator = globalThis.navigator as (Navigator & { wakeLock?: WakeLockLike }) | undefined;
  return maybeNavigator?.wakeLock;
}

function fullscreenSupported(root: HTMLElement): boolean {
  return document.fullscreenEnabled !== false && typeof root.requestFullscreen === 'function';
}

function applyCapabilityDataset(root: HTMLElement, state: UiCapabilityState): void {
  root.dataset.wakeLock = state.wakeLockActive ? 'active' : state.wakeLockRequested ? 'requested' : 'off';
  root.dataset.fullscreen = state.fullscreenActive ? 'active' : state.fullscreenRequested ? 'requested' : 'off';
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

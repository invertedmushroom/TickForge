export type MapLoadingHudState = {
  active: boolean;
  bundleId?: string;
  physicsStatus?: 'pending' | 'loading' | 'done' | 'error';
  physicsSource?: string;
  visualStatus?: 'pending' | 'loading' | 'done' | 'error';
  visualSource?: string;
  errorMessage?: string;
};

export type MapLoadingHudHandle = {
  setState: (state: MapLoadingHudState) => void;
  destroy: () => void;
};

export type MapLoadingHudOptions = {
  onRetry: () => void;
  onClearCache: () => void;
};

export function createMapLoadingHud(root: HTMLElement, options: MapLoadingHudOptions): MapLoadingHudHandle {
  root.innerHTML = `
    <section class="map-loading-hud" data-testid="map-loading-hud" hidden>
      <div class="map-loading-hud__container">
        <h2 class="map-loading-hud__title" data-testid="map-loading-title">Loading Map</h2>
        <div class="map-loading-hud__subtitle" data-testid="map-loading-subtitle"></div>
        
        <div class="map-loading-hud__progress-track">
          <div class="map-loading-hud__progress-fill" data-testid="map-loading-progress"></div>
        </div>

        <div class="map-loading-hud__details">
          <div class="map-loading-hud__detail-line" data-testid="map-loading-physics"></div>
          <div class="map-loading-hud__detail-line" data-testid="map-loading-visual"></div>
        </div>

        <div class="map-loading-hud__error" data-testid="map-loading-error" hidden></div>

        <div class="map-loading-hud__actions" data-testid="map-loading-actions" hidden>
          <button class="map-loading-hud__button map-loading-hud__button--retry" data-testid="map-loading-retry" type="button">Retry</button>
          <button class="map-loading-hud__button map-loading-hud__button--clear" data-testid="map-loading-clear" type="button">Clear Cache</button>
        </div>
      </div>
    </section>
  `;

  const hud = requiredChild<HTMLElement>(root, '[data-testid="map-loading-hud"]');
  const title = requiredChild<HTMLElement>(root, '[data-testid="map-loading-title"]');
  const subtitle = requiredChild<HTMLElement>(root, '[data-testid="map-loading-subtitle"]');
  const progress = requiredChild<HTMLElement>(root, '[data-testid="map-loading-progress"]');
  const physicsDetail = requiredChild<HTMLElement>(root, '[data-testid="map-loading-physics"]');
  const visualDetail = requiredChild<HTMLElement>(root, '[data-testid="map-loading-visual"]');
  const errorBox = requiredChild<HTMLElement>(root, '[data-testid="map-loading-error"]');
  const actions = requiredChild<HTMLElement>(root, '[data-testid="map-loading-actions"]');
  const retryBtn = requiredChild<HTMLButtonElement>(root, '[data-testid="map-loading-retry"]');
  const clearBtn = requiredChild<HTMLButtonElement>(root, '[data-testid="map-loading-clear"]');

  const onRetry = () => options.onRetry();
  const onClear = () => options.onClearCache();

  retryBtn.addEventListener('click', onRetry);
  clearBtn.addEventListener('click', onClear);

  return {
    setState: (state: MapLoadingHudState) => {
      hud.hidden = !state.active;
      if (!state.active) {
        return;
      }

      const isError = state.physicsStatus === 'error' || state.visualStatus === 'error';
      const isBlocking = state.physicsStatus !== 'done' && !isError;
      
      hud.className = 'map-loading-hud';
      if (isBlocking) {
        hud.classList.add('map-loading-hud--blocking');
      } else if (isError) {
        hud.classList.add('map-loading-hud--error');
      } else {
        hud.classList.add('map-loading-hud--toast');
      }

      title.textContent = isError ? 'Network Error' : isBlocking ? 'Preparing physics' : 'Loading visuals';
      subtitle.textContent = state.bundleId ?? '';

      // Set physics detail
      if (state.physicsStatus === 'done') {
        physicsDetail.textContent = `Physics: Ready (${state.physicsSource ?? 'unknown'})`;
        physicsDetail.className = 'map-loading-hud__detail-line map-loading-hud__detail-line--done';
      } else if (state.physicsStatus === 'loading') {
        physicsDetail.textContent = 'Physics: Fetching...';
        physicsDetail.className = 'map-loading-hud__detail-line map-loading-hud__detail-line--loading';
      } else {
        physicsDetail.textContent = '';
      }

      // Set visual detail
      if (state.visualStatus === 'done') {
        visualDetail.textContent = `Visuals: Ready (${state.visualSource ?? 'unknown'})`;
        visualDetail.className = 'map-loading-hud__detail-line map-loading-hud__detail-line--done';
      } else if (state.visualStatus === 'loading') {
        visualDetail.textContent = 'Visuals: Streaming...';
        visualDetail.className = 'map-loading-hud__detail-line map-loading-hud__detail-line--loading';
      } else {
        visualDetail.textContent = '';
      }

      // Handle progress bar
      if (isError) {
        progress.style.width = '100%';
        progress.className = 'map-loading-hud__progress-fill map-loading-hud__progress-fill--error';
      } else if (isBlocking) {
        progress.style.width = '30%'; // Fake progress for physics loading
        progress.className = 'map-loading-hud__progress-fill map-loading-hud__progress-fill--pulse';
      } else {
        progress.style.width = '70%'; // Visuals loading
        progress.className = 'map-loading-hud__progress-fill map-loading-hud__progress-fill--pulse';
      }

      // Handle Error UI
      if (state.errorMessage) {
        errorBox.textContent = state.errorMessage;
        errorBox.hidden = false;
        actions.hidden = false;
        hud.classList.add('map-loading-hud--error');
      } else {
        errorBox.hidden = true;
        actions.hidden = true;
      }
    },
    destroy: () => {
      retryBtn.removeEventListener('click', onRetry);
      clearBtn.removeEventListener('click', onClear);
      root.replaceChildren();
    },
  };
}

function requiredChild<T extends Element>(root: ParentNode, selector: string): T {
  const element = root.querySelector<T>(selector);
  if (!element) {
    throw new Error(`missing map loading HUD element ${selector}`);
  }
  return element;
}

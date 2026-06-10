import type { GameEventLogEntry } from '../events/gameEvents';
import type { StdbSnapshot } from '../stdb/connection';
import { ticksToSeconds } from '../timing';

export type GameplayHudModel = {
  healthText: string;
  healthRatio: number;
  isDead: boolean;
  deathText: string;
  respawnReady: boolean;
  eventLog: GameEventLogEntry[];
};

export type GameplayHudHandle = {
  render: (snapshot: StdbSnapshot) => void;
  destroy: () => void;
};

export type GameplayHudOptions = {
  onRespawn: () => void;
};

export function createGameplayHud(root: HTMLElement, options: GameplayHudOptions): GameplayHudHandle {
  root.innerHTML = `
    <section class="gameplay-hud__panel" data-testid="gameplay-hud">
      <div class="gameplay-hud__health">
        <div class="gameplay-hud__health-label" data-testid="local-health-status">HP --/--</div>
        <div class="gameplay-hud__health-track">
          <div class="gameplay-hud__health-fill" data-testid="local-health-fill"></div>
        </div>
      </div>
      <ol class="gameplay-hud__log" data-testid="combat-log"></ol>
    </section>
    <section class="death-overlay" data-testid="death-overlay" hidden>
      <div class="death-overlay__title">YOU DIED</div>
      <div class="death-overlay__detail" data-testid="death-countdown"></div>
      <button class="death-overlay__button" data-testid="respawn-button" type="button">Respawn</button>
    </section>
  `;

  const healthLabel = requiredChild<HTMLElement>(root, '[data-testid="local-health-status"]');
  const healthFill = requiredChild<HTMLElement>(root, '[data-testid="local-health-fill"]');
  const eventLog = requiredChild<HTMLOListElement>(root, '[data-testid="combat-log"]');
  const deathOverlay = requiredChild<HTMLElement>(root, '[data-testid="death-overlay"]');
  const deathCountdown = requiredChild<HTMLElement>(root, '[data-testid="death-countdown"]');
  const respawnButton = requiredChild<HTMLButtonElement>(root, '[data-testid="respawn-button"]');

  const onRespawn = () => options.onRespawn();
  respawnButton.addEventListener('click', onRespawn);

  return {
    render: (snapshot) => {
      const model = buildGameplayHudModel(snapshot);
      healthLabel.textContent = model.healthText;
      healthFill.style.width = `${Math.round(model.healthRatio * 100)}%`;
      deathOverlay.hidden = !model.isDead;
      deathCountdown.textContent = model.deathText;
      respawnButton.disabled = !model.respawnReady;
      eventLog.replaceChildren(...model.eventLog.map(renderLogEntry));
    },
    destroy: () => {
      respawnButton.removeEventListener('click', onRespawn);
      root.replaceChildren();
    },
  };
}

export function buildGameplayHudModel(snapshot: StdbSnapshot): GameplayHudModel {
  const health = snapshot.ownHealth;
  const maxHp = health?.maxHp ?? 0;
  const hp = health?.hp ?? 0;
  const healthRatio = maxHp > 0 ? Math.max(0, Math.min(1, hp / maxHp)) : 0;
  const isDead = snapshot.ownDeath !== undefined;
  const currentTick = snapshot.latestTick;
  const respawnAtTick = snapshot.ownDeath?.respawnAtTick;
  const remainingTicks =
    currentTick !== undefined && respawnAtTick !== undefined && respawnAtTick > currentTick
      ? respawnAtTick - currentTick
      : 0n;

  return {
    healthText: maxHp > 0 ? `HP ${Math.ceil(hp)}/${Math.ceil(maxHp)}` : 'HP --/--',
    healthRatio,
    isDead,
    deathText: isDead ? deathText(remainingTicks) : '',
    respawnReady: isDead && remainingTicks === 0n,
    eventLog: snapshot.gameEventLog,
  };
}

function deathText(remainingTicks: bigint): string {
  if (remainingTicks <= 0n) {
    return 'Respawn ready';
  }
  return `Respawn in ${ticksToSeconds(remainingTicks).toFixed(1)}s`;
}

function renderLogEntry(entry: GameEventLogEntry): HTMLLIElement {
  const item = document.createElement('li');
  item.className = `gameplay-hud__log-entry gameplay-hud__log-entry--${entry.tone}`;
  item.textContent = entry.text;
  item.dataset.key = entry.key;
  return item;
}

function requiredChild<T extends Element>(root: ParentNode, selector: string): T {
  const element = root.querySelector<T>(selector);
  if (!element) {
    throw new Error(`missing gameplay HUD element ${selector}`);
  }
  return element;
}

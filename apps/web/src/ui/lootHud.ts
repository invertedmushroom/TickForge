import type { StdbClient, StdbSnapshot, LootPile, LootPileItem, PlayerInventory } from '../stdb/connection';
import type { UiOverlayHandle, UiRuntime } from './runtime';

const LOOT_FEATURE_QUERIES = [
  'SELECT * FROM loot_pile',
  'SELECT * FROM loot_pile_item',
  'SELECT * FROM player_inventory',
] as const;

export const LOOT_CLAIM_MAX_DISTANCE_M = 4.0;
const INVENTORY_SLOT_COUNT = 64;

export type LootPileView = {
  pile: LootPile;
  items: LootPileItem[];
  canClaim: boolean;
  distanceMeters?: number;
  expiresInTicks?: bigint;
};

export type LootHudModel = {
  piles: LootPileView[];
  inventory: PlayerInventory[];
  freeSlot?: number;
};

export type LootHudHandle = {
  destroy: () => void;
};

export function createLootHud(
  root: HTMLElement,
  options: { stdbClient: StdbClient; runtime: UiRuntime },
): LootHudHandle {
  const host = document.createElement('section');
  host.className = 'loot-hud';
  host.dataset.testid = 'loot-hud';

  const marker = document.createElement('button');
  marker.type = 'button';
  marker.className = 'loot-hud__marker';
  marker.dataset.testid = 'loot-marker';
  marker.setAttribute('aria-label', 'Open loot');

  const status = document.createElement('div');
  status.className = 'loot-hud__status';
  status.dataset.testid = 'loot-status';
  status.setAttribute('aria-live', 'polite');

  host.append(marker, status);
  root.append(host);

  let latestSnapshot = options.stdbClient.snapshot();
  let panel: UiOverlayHandle | undefined;
  let panelBody: HTMLElement | undefined;
  let statusText = '';

  const releaseSubscriptions = options.stdbClient.activateFeatureSubscriptions(
    'loot-hud',
    LOOT_FEATURE_QUERIES,
  );

  const render = (snapshot: StdbSnapshot) => {
    latestSnapshot = snapshot;
    const model = buildLootHudModel(snapshot);
    renderMarker(marker, model);
    renderPanel(model);
  };

  const setStatus = (message: string) => {
    statusText = message;
    status.textContent = statusText;
    renderPanel(buildLootHudModel(latestSnapshot));
  };

  const claim = async (entry: LootPileView, item: LootPileItem) => {
    if (!entry.canClaim) {
      setStatus('Loot out of range');
      return;
    }
    const slot = firstFreeInventorySlot(latestSnapshot.ownInventory);
    if (slot === undefined) {
      setStatus('Inventory full');
      return;
    }
    setStatus(`Claiming item ${item.itemId}`);
    try {
      await options.stdbClient.claimLoot(entry.pile.lootPileId, item.itemId, slot);
      setStatus(`Item ${item.itemId} claimed`);
    } catch (error) {
      setStatus(`Claim failed: ${errorMessage(error)}`);
    }
  };

  const openLootPanel = () => {
    if (panel?.isOpen()) {
      panel.close();
      panel = undefined;
      panelBody = undefined;
      return;
    }
    const body = document.createElement('div');
    body.className = 'loot-panel';
    body.dataset.testid = 'loot-panel-body';
    panel = options.runtime.openPanel({
      id: 'loot-claim',
      title: 'Loot',
      content: body,
    });
    panelBody = body;
    renderPanel(buildLootHudModel(latestSnapshot));
  };

  const renderPanel = (model: LootHudModel) => {
    if (!panel?.isOpen() || !panelBody) {
      return;
    }
    panelBody.replaceChildren(renderPanelContents(model, claim, statusText));
  };

  marker.addEventListener('click', openLootPanel);
  const unsubscribe = options.stdbClient.subscribe(render);
  render(latestSnapshot);

  return {
    destroy: () => {
      marker.removeEventListener('click', openLootPanel);
      unsubscribe();
      releaseSubscriptions();
      panel?.close();
      host.remove();
    },
  };
}

export function buildLootHudModel(snapshot: StdbSnapshot): LootHudModel {
  const entityId = snapshot.entityId;
  const layer = snapshot.layer;
  const currentTick = snapshot.latestTick;
  const ownPosition = snapshot.ownTransform;
  const piles =
    entityId === undefined || layer === undefined
      ? []
      : snapshot.lootPiles
          .filter((pile) => pile.layer === layer)
          .filter((pile) => pile.eligibleClaimants.includes(entityId))
          .filter((pile) => currentTick === undefined || currentTick < pile.expiresAtTick)
          .map((pile) => {
            const items = snapshot.lootPileItems.filter((item) => item.lootPileId === pile.lootPileId);
            const distanceMeters = ownPosition ? distanceToPile(ownPosition, pile) : undefined;
            return {
              pile,
              items,
              canClaim: distanceMeters !== undefined && distanceMeters <= LOOT_CLAIM_MAX_DISTANCE_M,
              distanceMeters,
              expiresInTicks:
                currentTick !== undefined && pile.expiresAtTick > currentTick
                  ? pile.expiresAtTick - currentTick
                  : undefined,
            };
          })
          .filter((entry) => entry.items.length > 0)
          .sort((a, b) => (a.distanceMeters ?? Number.POSITIVE_INFINITY) - (b.distanceMeters ?? Number.POSITIVE_INFINITY));

  return {
    piles,
    inventory: snapshot.ownInventory,
    freeSlot: firstFreeInventorySlot(snapshot.ownInventory),
  };
}

export function firstFreeInventorySlot(inventory: readonly PlayerInventory[], slotCount = INVENTORY_SLOT_COUNT): number | undefined {
  const occupied = new Set(inventory.map((row) => row.slotIndex));
  for (let slot = 0; slot < slotCount; slot += 1) {
    if (!occupied.has(slot)) {
      return slot;
    }
  }
  return undefined;
}

function renderMarker(marker: HTMLButtonElement, model: LootHudModel): void {
  marker.hidden = model.piles.length === 0;
  marker.disabled = model.piles.length === 0;
  if (model.piles.length === 0) {
    marker.textContent = '';
    return;
  }
  const itemCount = model.piles.reduce((count, pile) => count + pile.items.length, 0);
  const nearest = model.piles[0]?.distanceMeters;
  marker.textContent = nearest === undefined ? `Loot ${itemCount}` : `Loot ${itemCount} · ${nearest.toFixed(1)}m`;
}

function renderPanelContents(
  model: LootHudModel,
  claim: (entry: LootPileView, item: LootPileItem) => void,
  statusText: string,
): HTMLElement {
  const body = document.createElement('div');
  body.className = 'loot-panel__content';

  if (statusText) {
    const status = document.createElement('div');
    status.className = 'loot-panel__status';
    status.dataset.testid = 'loot-panel-status';
    status.textContent = statusText;
    body.append(status);
  }

  if (model.piles.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'loot-panel__empty';
    empty.textContent = 'No eligible loot';
    body.append(empty);
    return body;
  }

  for (const entry of model.piles) {
    const pile = document.createElement('section');
    pile.className = 'loot-panel__pile';
    pile.dataset.testid = 'loot-panel-pile';

    const title = document.createElement('h3');
    title.className = 'loot-panel__pile-title';
    title.textContent = pileTitle(entry);
    pile.append(title);

    const list = document.createElement('div');
    list.className = 'loot-panel__items';
    for (const item of entry.items) {
      const row = document.createElement('div');
      row.className = 'loot-panel__item';

      const label = document.createElement('div');
      label.className = 'loot-panel__item-label';
      label.textContent = `Item ${item.itemId} x${item.quantity}`;

      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'loot-panel__claim';
      button.dataset.testid = 'loot-claim-button';
      button.textContent = 'Claim';
      button.disabled = model.freeSlot === undefined || !entry.canClaim;
      button.title = entry.canClaim ? 'Claim loot' : 'Loot out of range';
      button.addEventListener('click', () => claim(entry, item));

      row.append(label, button);
      list.append(row);
    }
    pile.append(list);
    body.append(pile);
  }

  const inventory = document.createElement('div');
  inventory.className = 'loot-panel__inventory';
  inventory.dataset.testid = 'loot-inventory-summary';
  inventory.textContent = `Inventory ${model.inventory.length}/${INVENTORY_SLOT_COUNT}`;
  body.append(inventory);

  return body;
}

function pileTitle(entry: LootPileView): string {
  const distance = entry.distanceMeters === undefined ? '' : ` · ${entry.distanceMeters.toFixed(1)}m`;
  const expires = entry.expiresInTicks === undefined ? '' : ` · ${entry.expiresInTicks} ticks`;
  return `Pile ${entry.pile.lootPileId}${distance}${expires}`;
}

function distanceToPile(position: { posX: number; posY: number; posZ: number }, pile: LootPile): number {
  const dx = position.posX - pile.posX;
  const dy = position.posY - pile.posY;
  const dz = position.posZ - pile.posZ;
  return Math.sqrt(dx * dx + dy * dy + dz * dz);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

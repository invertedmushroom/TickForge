export type InteractLootModel = {
  freeSlot?: number;
  piles: readonly {
    canClaim?: boolean;
    pile: { lootPileId: bigint };
    items: readonly { itemId: number }[];
  }[];
};

export type InteractTarget =
  | { kind: 'entity'; entityId: bigint }
  | { kind: 'loot'; lootPileId: bigint; itemId: number; targetSlot: number };

export type InteractTargetInput = {
  selectedEntity?: bigint;
  hoverEntity?: bigint;
  loot?: InteractLootModel;
};

export function resolveInteractTarget(input: InteractTargetInput): InteractTarget | undefined {
  const entityId = input.selectedEntity ?? input.hoverEntity;
  if (entityId !== undefined) {
    return { kind: 'entity', entityId };
  }

  const targetSlot = input.loot?.freeSlot;
  if (targetSlot === undefined) {
    return undefined;
  }

  const pile = input.loot?.piles.find((entry) => entry.canClaim !== false);
  const item = pile?.items[0];
  if (!pile || !item) {
    return undefined;
  }

  return {
    kind: 'loot',
    lootPileId: pile.pile.lootPileId,
    itemId: item.itemId,
    targetSlot,
  };
}

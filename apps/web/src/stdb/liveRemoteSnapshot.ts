type RemoteTransformLike = {
  entityId: bigint;
};

type RemoteEntityLike = {
  entityId: bigint;
  state: {
    tag: string;
  };
};

export function filterLiveRemoteSnapshot<TTransform extends RemoteTransformLike, TEntity extends RemoteEntityLike>(
  transforms: readonly TTransform[],
  entities: Iterable<TEntity>,
  ownEntityId: bigint | undefined,
): { remoteTransforms: TTransform[]; remoteEntities: Map<bigint, TEntity> } {
  const remoteEntities = new Map<bigint, TEntity>();
  const terminalRemoteEntityIds = new Set<bigint>();
  for (const row of entities) {
    if (row.entityId === ownEntityId) {
      continue;
    }
    if (isTerminalEntity(row)) {
      terminalRemoteEntityIds.add(row.entityId);
    } else {
      remoteEntities.set(row.entityId, row);
    }
  }
  const remoteTransforms = transforms.filter((row) => {
    if (row.entityId === ownEntityId) {
      return false;
    }
    return !terminalRemoteEntityIds.has(row.entityId);
  });
  return { remoteTransforms, remoteEntities };
}

function isTerminalEntity(entity: RemoteEntityLike): boolean {
  return entity.state.tag === 'DespawnPending' || entity.state.tag === 'Removed';
}

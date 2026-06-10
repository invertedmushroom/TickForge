import { mapBundles, type BundleCollider, type MapBundleEntry } from '@dive/client-contract/map-bundles';

export const MAP_COLLIDER_FORMAT_VERSION = 1;

export type LoadedMapBundle = {
  bundleId: string;
  manifest: MapBundleEntry['manifest'];
  colliders: MapBundleEntry['colliders'];
};

export type RuntimeMapState = {
  layer?: number;
  entityLayer?: { layer: number };
  ownInstance?: { templateId: string };
};

export type { BundleCollider, MapBundleEntry };

export class MapBundleError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'MapBundleError';
  }
}

export class MapBundleMismatchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'MapBundleMismatchError';
  }
}

export function getMapBundleEntryForLayer(layerId: number): MapBundleEntry {
  const entry = mapBundles.bundles.find(
    (candidate) => candidate.manifest.source.kind === 'layer' && candidate.manifest.source.layer_id === layerId,
  );
  if (!entry) {
    throw new MapBundleError(`no map bundle for layer ${layerId}`);
  }
  return entry;
}

export function getMapBundleEntryForDungeon(templateId: string): MapBundleEntry {
  const entry = mapBundles.bundles.find(
    (candidate) =>
      candidate.manifest.source.kind === 'dungeon' && candidate.manifest.source.dungeon_template_id === templateId,
  );
  if (!entry) {
    throw new MapBundleError(`no map bundle for dungeon template ${templateId}`);
  }
  return entry;
}

export function getStartupMapBundleEntry(): MapBundleEntry {
  return getMapBundleEntryForLayer(0);
}

export function getRuntimeMapBundleEntry(state: RuntimeMapState): MapBundleEntry {
  if (state.ownInstance?.templateId) {
    return getMapBundleEntryForDungeon(state.ownInstance.templateId);
  }
  return getMapBundleEntryForLayer(state.entityLayer?.layer ?? state.layer ?? 0);
}

export function getMapBundleForLayer(layerId: number, expectedContentHash?: string): LoadedMapBundle {
  const entry = getMapBundleEntryForLayer(layerId);
  return validateMapBundle(entry, expectedContentHash);
}

export function getMapBundleForDungeon(templateId: string, expectedContentHash?: string): LoadedMapBundle {
  const entry = getMapBundleEntryForDungeon(templateId);
  return validateMapBundle(entry, expectedContentHash);
}

export function getStartupMapBundle(expectedContentHash?: string): LoadedMapBundle {
  return validateMapBundle(getStartupMapBundleEntry(), expectedContentHash);
}

export function getRuntimeMapBundle(state: RuntimeMapState, expectedContentHash?: string): LoadedMapBundle {
  return validateMapBundle(getRuntimeMapBundleEntry(state), expectedContentHash);
}

export function validateMapBundle(entry: MapBundleEntry, expectedContentHash = entry.manifest.content_hash): LoadedMapBundle {
  if (entry.manifest.content_hash !== expectedContentHash) {
    throw new MapBundleMismatchError(
      `map bundle content mismatch: expected ${expectedContentHash}, got ${entry.manifest.content_hash}`,
    );
  }

  if (entry.colliders.content_hash !== expectedContentHash) {
    throw new MapBundleMismatchError(
      `collider content mismatch: expected ${expectedContentHash}, got ${entry.colliders.content_hash}`,
    );
  }

  if (entry.colliders.format_version !== MAP_COLLIDER_FORMAT_VERSION) {
    throw new MapBundleError(
      `unsupported collider format ${entry.colliders.format_version}; expected ${MAP_COLLIDER_FORMAT_VERSION}`,
    );
  }

  if (entry.manifest.bundle_id !== entry.bundle_id) {
    throw new MapBundleError(`bundle id mismatch: index ${entry.bundle_id}, manifest ${entry.manifest.bundle_id}`);
  }

  return {
    bundleId: entry.bundle_id,
    manifest: entry.manifest,
    colliders: entry.colliders,
  };
}

export function mapBundleSummary(bundle: LoadedMapBundle): string {
  const source = bundle.manifest.source;
  const sourceId =
    source.kind === 'layer' ? `layer ${source.layer_id ?? 'unknown'}` : `dungeon ${source.dungeon_template_id ?? 'unknown'}`;
  return `${sourceId} · ${bundle.manifest.bundle_version} · colliders ${bundle.colliders.colliders.length}`;
}

import {
  composeKindBaseUrl,
  createAssetFetcher,
  resolveAssetBaseUrl,
  type AssetCacheInfo,
  type AssetFetcher,
} from '../assets/assetFetcher.ts';
import type { Group } from 'three';
import {
  getRuntimeMapBundleEntry,
  getStartupMapBundleEntry,
  validateMapBundle,
  type LoadedMapBundle,
  type MapBundleEntry,
  type RuntimeMapState,
  type TerrainCollisionGeometry,
} from './content';

export const MAP_ASSET_KIND = 'map-bundles';
const MAP_ASSET_CACHE_NAME = 'dive-map-assets-v1';
const VISUAL_ENTRY_PATTERN = /\.(gltf|glb)$/i;

// The map-asset module re-exports the generic cache types under map-flavored
// names so consumers (sceneManager, diagnostics) only import from one place.
export type MapAssetCacheInfo = AssetCacheInfo;
export type MapAssetCacheStatus = AssetCacheInfo['status'];
export type MapAssetSource = AssetCacheInfo['source'];

export type LoadedPhysicsMap = {
  bundle: LoadedMapBundle;
  cache: MapAssetCacheInfo;
};

export type LoadedVisualMap = {
  bundleId: string;
  contentHash: string;
  /** Three.js scene graph root for the loaded glTF; consumers add it to their scene. */
  group: Group;
  /** Releases the parsed glTF scene + any blob URLs the loader minted. */
  dispose: () => void;
  cache: MapAssetCacheInfo;
};

/**
 * Handles for an in-flight map load. Physics resolves first and gates gameplay;
 * visual resolves independently and may resolve to `undefined` when the bundle
 * has no `visual_content_hash` (the common case today).
 */
export type MapAssetHandles = {
  bundleId: string;
  physics: Promise<LoadedPhysicsMap>;
  visual: Promise<LoadedVisualMap | undefined>;
};

export type MapAssetProvider = {
  resolveStartupBundleId: () => string;
  resolveRuntimeBundleId: (state: RuntimeMapState) => string;
  loadStartupMap: () => MapAssetHandles;
  loadRuntimeMap: (state: RuntimeMapState) => MapAssetHandles;
  clearCache: () => Promise<void>;
};

export type MapAssetProviderOptions = {
  /**
   * Kind-scoped CDN base, e.g. `https://cdn.example/map-bundles/`.
   * Inferred from `VITE_ASSET_BASE_URL` (+ `map-bundles/` segment) if omitted.
   */
  baseUrl?: string;
  /** Required `content_hash` for the physics determinism gate. */
  expectedContentHash?: string;
  /** When `true` (default), falls back to embedded fixtures if the CDN fetch fails. */
  embeddedFallback?: boolean;
  /** Test seam: override the underlying asset fetcher. */
  fetcher?: AssetFetcher;
};

export class MapAssetLoadError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'MapAssetLoadError';
  }
}

export function createMapAssetProvider(options: MapAssetProviderOptions = {}): MapAssetProvider {
  const baseUrl = options.baseUrl ?? composeKindBaseUrl(resolveAssetBaseUrl(), MAP_ASSET_KIND);
  const embeddedFallback = options.embeddedFallback ?? true;
  const fetcher = options.fetcher ?? (baseUrl
    ? createAssetFetcher({ baseUrl, cacheName: MAP_ASSET_CACHE_NAME })
    : undefined);

  const physicsMemory = new Map<string, LoadedPhysicsMap>();
  const visualMemory = new Map<string, LoadedVisualMap | undefined>();

  function loadEntry(entry: MapBundleEntry): MapAssetHandles {
    const physicsKey = `${entry.bundle_id}|${options.expectedContentHash ?? entry.manifest.content_hash}`;
    const visualKey = `${entry.bundle_id}|${entry.manifest.visual_content_hash ?? 'none'}`;

    const physics = withCache(physicsMemory, physicsKey, withMemoryHitPhysics, () =>
      loadPhysicsChannel(entry, fetcher, options.expectedContentHash, embeddedFallback),
    );

    const visual = withCache(visualMemory, visualKey, withMemoryHitVisualOptional, () =>
      loadVisualChannel(entry, fetcher),
    );

    return { bundleId: entry.bundle_id, physics, visual };
  }

  return {
    resolveStartupBundleId: () => getStartupMapBundleEntry().bundle_id,
    resolveRuntimeBundleId: (state) => getRuntimeMapBundleEntry(state).bundle_id,
    loadStartupMap: () => loadEntry(getStartupMapBundleEntry()),
    loadRuntimeMap: (state) => loadEntry(getRuntimeMapBundleEntry(state)),
    async clearCache() {
      physicsMemory.clear();
      visualMemory.clear();
      if (fetcher) {
        await fetcher.clearCache();
      }
    },
  };
}

function withCache<V>(
  memory: Map<string, V>,
  key: string,
  onHit: (cached: V) => V,
  load: () => Promise<V>,
): Promise<V> {
  if (memory.has(key)) {
    return Promise.resolve(onHit(memory.get(key) as V));
  }
  return load().then((loaded) => {
    memory.set(key, loaded);
    return loaded;
  });
}

async function loadPhysicsChannel(
  entry: MapBundleEntry,
  fetcher: AssetFetcher | undefined,
  expectedContentHash: string | undefined,
  embeddedFallback: boolean,
): Promise<LoadedPhysicsMap> {
  if (!fetcher) {
    return embeddedPhysics(entry, expectedContentHash);
  }
  try {
    return await loadRemotePhysics(entry, fetcher, expectedContentHash);
  } catch (error) {
    if (!embeddedFallback) {
      throw error;
    }
    return embeddedPhysics(entry, expectedContentHash, errorMessage(error));
  }
}

async function loadRemotePhysics(
  entry: MapBundleEntry,
  fetcher: AssetFetcher,
  expectedContentHash: string | undefined,
): Promise<LoadedPhysicsMap> {
  const manifestResult = await fetcher.fetchJson<MapBundleEntry['manifest']>(`${entry.bundle_id}/manifest.json`);
  const manifest = manifestResult.value;
  const colliderRef = manifest.collider_json[0];
  if (!colliderRef) {
    throw new MapAssetLoadError(`map bundle ${entry.bundle_id} has no collider json reference`);
  }
  const colliderResult = await fetcher.fetchJson<MapBundleEntry['colliders']>(
    `${entry.bundle_id}/${colliderRef.url}`,
    colliderRef.sha256,
  );
  const bundle = validateMapBundle(
    { bundle_id: entry.bundle_id, manifest, colliders: colliderResult.value } as MapBundleEntry,
    expectedContentHash,
  );
  bundle.terrainCollision = await loadTerrainCollision(entry.bundle_id, manifest, fetcher);
  const networkTouched = manifestResult.source === 'network' || colliderResult.source === 'network';
  return {
    bundle,
    cache: {
      source: networkTouched ? 'network' : 'disk',
      status: networkTouched ? 'stored' : 'hit',
      bytes: manifestResult.bytes + colliderResult.bytes,
    },
  };
}

function embeddedPhysics(
  entry: MapBundleEntry,
  expectedContentHash: string | undefined,
  fallbackReason?: string,
): LoadedPhysicsMap {
  return {
    bundle: validateMapBundle(entry, expectedContentHash),
    cache: {
      source: 'embedded',
      status: fallbackReason ? 'fallback' : 'embedded',
      bytes: embeddedBundleBytes(entry),
      detail: fallbackReason,
    },
  };
}

async function loadVisualChannel(
  entry: MapBundleEntry,
  fetcher: AssetFetcher | undefined,
): Promise<LoadedVisualMap | undefined> {
  const visualHash = entry.manifest.visual_content_hash;
  if (!visualHash || entry.manifest.render_meshes.length === 0) {
    return undefined;
  }
  if (!fetcher) {
    throw new MapAssetLoadError(
      `visual bundle ${entry.bundle_id} requires an asset base URL (none configured)`,
    );
  }
  const entryAsset = entry.manifest.render_meshes.find((asset) =>
    VISUAL_ENTRY_PATTERN.test(asset.url),
  );
  if (!entryAsset) {
    throw new MapAssetLoadError(
      `visual bundle ${entry.bundle_id} has no .gltf/.glb entry in render_meshes`,
    );
  }

  const entryUrl = fetcher.resolveUrl(`${entry.bundle_id}/${entryAsset.url}`);
  // Three's GLTFLoader resolves `.bin` and texture references relative to the
  // glTF URL. The dev middleware and production CDN both serve those siblings
  // at predictable paths, so we just need GLTFLoader to fetch them in turn.
  const { GLTFLoader } = await import('three/examples/jsm/loaders/GLTFLoader.js');
  const loader = new GLTFLoader();
  const gltf = await loader.loadAsync(entryUrl);

  const totalBytes = entry.manifest.render_meshes.reduce((sum, asset) => sum + asset.bytes, 0);
  return {
    bundleId: entry.bundle_id,
    contentHash: visualHash,
    group: gltf.scene,
    dispose: () => disposeGltfScene(gltf.scene),
    cache: {
      // TODO: Three's loader bypasses our Cache Storage path, so the best we can
      // report is that the network had to be touched. A future iteration can
      // route the loader's resource manager through `fetcher.fetchJson` and
      // surface a real hit/stored breakdown.
      source: 'network',
      status: 'bypassed',
      bytes: totalBytes,
    },
  };
}

/**
 * Load the terrain collision mesh referenced by `manifest.collision_mesh`, weld
 * it into world-space trimesh geometry, and apply the import transform so KCC
 * prediction collides against the same low-res mesh the worker imported into the
 * DB. Returns `undefined` for RON-only bundles or when the glTF fails to load;
 * the caller degrades to RON colliders (server stays authoritative).
 */
async function loadTerrainCollision(
  bundleId: string,
  manifest: MapBundleEntry['manifest'],
  fetcher: AssetFetcher,
): Promise<TerrainCollisionGeometry | undefined> {
  const ref = manifest.collision_mesh;
  if (!ref) {
    return undefined;
  }
  try {
    const entryUrl = fetcher.resolveUrl(`${bundleId}/${ref.entry_url}`);
    const { GLTFLoader } = await import('three/examples/jsm/loaders/GLTFLoader.js');
    const loader = new GLTFLoader();
    const gltf = await loader.loadAsync(entryUrl);
    const scenes = gltf.scenes.length > 0 ? gltf.scenes : [gltf.scene];
    const geometry = extractTrimesh(scenes);
    for (const scene of scenes) {
      disposeGltfScene(scene);
    }
    if (!geometry) {
      console.warn(`collision mesh ${bundleId} has no triangle geometry; using RON colliders only`);
      return undefined;
    }
    applyCollisionTransform(geometry, ref.transform);
    return geometry;
  } catch (error) {
    console.warn(`collision mesh ${bundleId} failed to load; using RON colliders only`, error);
    return undefined;
  }
}

/**
 * Flatten every mesh in every loaded glTF scene into a single world-space trimesh,
 * baking node/mesh transforms into the vertices. Mirrors the worker's
 * `load_gltf_triangles` so client and server derive identical source geometry.
 */
function extractTrimesh(roots: readonly Group[]): TerrainCollisionGeometry | undefined {
  const positions: number[] = [];
  const indices: number[] = [];
  let vertexOffset = 0;
  for (const root of roots) {
    root.updateMatrixWorld(true);
    root.traverse((object) => {
      const mesh = object as {
        isMesh?: boolean;
        geometry?: {
          clone: () => {
            applyMatrix4: (matrix: unknown) => void;
            getAttribute: (
              name: string,
            ) =>
              | { count: number; getX: (i: number) => number; getY: (i: number) => number; getZ: (i: number) => number }
              | undefined;
            getIndex: () => { count: number; getX: (i: number) => number } | null;
            dispose: () => void;
          };
        };
        matrixWorld?: unknown;
      };
      if (!mesh.isMesh || !mesh.geometry) {
        return;
      }
      const geometry = mesh.geometry.clone();
      if (mesh.matrixWorld) {
        geometry.applyMatrix4(mesh.matrixWorld);
      }
      const position = geometry.getAttribute('position');
      if (!position) {
        geometry.dispose();
        return;
      }
      const count = position.count;
      for (let i = 0; i < count; i += 1) {
        positions.push(position.getX(i), position.getY(i), position.getZ(i));
      }
      const index = geometry.getIndex();
      if (index) {
        for (let i = 0; i < index.count; i += 1) {
          indices.push(vertexOffset + index.getX(i));
        }
      } else {
        for (let i = 0; i < count; i += 1) {
          indices.push(vertexOffset + i);
        }
      }
      vertexOffset += count;
      geometry.dispose();
    });
  }
  if (positions.length < 9 || indices.length < 3) {
    return undefined;
  }
  return { vertices: new Float32Array(positions), indices: new Uint32Array(indices) };
}

/**
 * Apply the manifest import transform in place: `v = v * scale + offset`, then
 * swap each triangle's 2nd/3rd index when `flip_winding`. Matches the worker's
 * `apply_terrain_transform`.
 */
function applyCollisionTransform(
  geometry: TerrainCollisionGeometry,
  transform: NonNullable<MapBundleEntry['manifest']['collision_mesh']>['transform'],
): void {
  const scale = transform.scale;
  const ox = transform.offset[0] ?? 0;
  const oy = transform.offset[1] ?? 0;
  const oz = transform.offset[2] ?? 0;
  const v = geometry.vertices;
  for (let i = 0; i + 2 < v.length; i += 3) {
    v[i] = v[i] * scale + ox;
    v[i + 1] = v[i + 1] * scale + oy;
    v[i + 2] = v[i + 2] * scale + oz;
  }
  if (transform.flip_winding) {
    const idx = geometry.indices;
    for (let i = 0; i + 2 < idx.length; i += 3) {
      const tmp = idx[i + 1];
      idx[i + 1] = idx[i + 2];
      idx[i + 2] = tmp;
    }
  }
}

function disposeGltfScene(group: Group): void {
  group.traverse((object) => {
    const mesh = object as { geometry?: { dispose?: () => void }; material?: unknown };
    mesh.geometry?.dispose?.();
    const materials = Array.isArray(mesh.material) ? mesh.material : mesh.material ? [mesh.material] : [];
    for (const material of materials as Array<{ dispose?: () => void; map?: { dispose?: () => void } }>) {
      material.map?.dispose?.();
      material.dispose?.();
    }
  });
  group.removeFromParent();
}

function withMemoryHitPhysics(loaded: LoadedPhysicsMap): LoadedPhysicsMap {
  return { bundle: loaded.bundle, cache: rewriteAsMemoryHit(loaded.cache) };
}

function withMemoryHitVisualOptional(loaded: LoadedVisualMap | undefined): LoadedVisualMap | undefined {
  return loaded ? { ...loaded, cache: rewriteAsMemoryHit(loaded.cache) } : undefined;
}

function rewriteAsMemoryHit(cache: MapAssetCacheInfo): MapAssetCacheInfo {
  return { source: 'memory', status: 'hit', bytes: cache.bytes, detail: cache.detail };
}

function embeddedBundleBytes(entry: MapBundleEntry): number {
  return new TextEncoder().encode(JSON.stringify(entry.manifest) + JSON.stringify(entry.colliders)).byteLength;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

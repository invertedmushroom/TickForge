/**
 * Generic CDN-shaped asset fetcher used by every asset kind (maps, rigs, effects, ...).
 *
 * Bundle layout convention (mirrors `target/web-contract/<kind>/` on disk):
 *
 *     <asset_base_url>/<kind>/<bundle_id>/<relative_path>
 *
 * Each asset kind owns its own validation (determinism gates, schema checks, etc.)
 * and supplies the expected SHA-256 per asset; this module only handles transport,
 * Cache Storage persistence, integrity verification, and source/status reporting.
 *
 * Do NOT bake map-specific logic in here. Adding rigs, VFX, audio, etc. should be
 * a matter of constructing a new fetcher with a different `cacheName`.
 */

const DEV_ASSET_BASE_URL = '/__dive_assets__/';

export type AssetSource = 'memory' | 'disk' | 'network' | 'embedded';
export type AssetCacheStatus = 'hit' | 'stored' | 'embedded' | 'fallback' | 'bypassed';

export type AssetCacheInfo = {
  source: AssetSource;
  status: AssetCacheStatus;
  bytes: number;
  detail?: string;
};

export type AssetJsonResult<T> = {
  value: T;
  source: Exclude<AssetSource, 'embedded'>;
  bytes: number;
};

export type AssetFetcher = {
  /** Returns the absolute URL for a bundle-relative path. */
  resolveUrl: (relativePath: string) => string;
  fetchJson: <T = unknown>(relativePath: string, expectedSha256?: string) => Promise<AssetJsonResult<T>>;
  clearCache: () => Promise<void>;
};

export type AssetFetcherOptions = {
  /**
   * Kind-scoped base URL, e.g. `https://cdn.example/map-bundles/` or
   * `/__dive_assets__/map-bundles/`. The kind segment must already be present;
   * use `composeKindBaseUrl` to compose it from the global asset base.
   */
  baseUrl: string;
  /**
   * Cache Storage bucket name. Use one bucket per asset kind so eviction and
   * invalidation can be reasoned about kind-by-kind.
   */
  cacheName: string;
  /** Test seam: override the underlying transport. Defaults to `fetch`. */
  fetchImpl?: (url: string) => Promise<Response>;
};

export class AssetFetchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'AssetFetchError';
  }
}

export function createAssetFetcher(options: AssetFetcherOptions): AssetFetcher {
  const baseUrl = normalizeBaseUrl(options.baseUrl);
  const fetchImpl = options.fetchImpl ?? defaultFetch;

  return {
    resolveUrl: (relativePath) => joinBundleUrl(baseUrl, relativePath),
    fetchJson: <T>(relativePath: string, expectedSha256?: string) =>
      fetchJsonWithCache<T>(joinBundleUrl(baseUrl, relativePath), expectedSha256, options.cacheName, fetchImpl),
    clearCache: async () => {
      if (cacheStorageAvailable()) {
        await caches.delete(options.cacheName);
      }
    },
  };
}

/**
 * Resolve the global asset base URL from environment.
 *
 * Production: requires `VITE_ASSET_BASE_URL`. Without it, asset loading
 * is disabled and callers must fall back to their own strategy (e.g. embedded
 * fixtures for maps).
 * Development: defaults to the local Vite middleware path.
 */
export function resolveAssetBaseUrl(): string | undefined {
  const configured = (import.meta.env.VITE_ASSET_BASE_URL as string | undefined)?.trim();
  if (configured) {
    return ensureTrailingSlash(configured);
  }
  return import.meta.env.DEV ? DEV_ASSET_BASE_URL : undefined;
}

/** Compose a per-kind base URL by appending `<kind>/` to the global asset base. */
export function composeKindBaseUrl(assetBase: string | undefined, kind: string): string | undefined {
  if (!assetBase) {
    return undefined;
  }
  // Resolve against the document origin so root-relative bases (`/__dive_assets__/`)
  // can serve as the `URL` constructor's `base` argument.
  const absoluteBase = new URL(ensureTrailingSlash(assetBase), documentOrigin()).toString();
  return ensureTrailingSlash(new URL(`${kind}/`, absoluteBase).toString());
}

async function fetchJsonWithCache<T>(
  url: string,
  expectedSha256: string | undefined,
  cacheName: string,
  fetchImpl: (url: string) => Promise<Response>,
): Promise<AssetJsonResult<T>> {
  if (cacheStorageAvailable()) {
    const cache = await caches.open(cacheName);
    const request = new Request(url, { credentials: 'same-origin' });
    const cached = await cache.match(request);
    if (cached) {
      return parseJsonResponse<T>(cached, 'disk', expectedSha256);
    }
    const response = await requireOk(fetchImpl(url), url);
    const cacheable = response.clone();
    const parsed = await parseJsonResponse<T>(response, 'network', expectedSha256);
    await cache.put(request, cacheable);
    return parsed;
  }

  const response = await requireOk(fetchImpl(url), url);
  return parseJsonResponse<T>(response, 'network', expectedSha256);
}

async function requireOk(responsePromise: Promise<Response>, url: string): Promise<Response> {
  const response = await responsePromise;
  if (!response.ok) {
    throw new AssetFetchError(`asset fetch failed ${response.status}: ${url}`);
  }
  return response;
}

async function parseJsonResponse<T>(
  response: Response,
  source: Exclude<AssetSource, 'embedded'>,
  expectedSha256?: string,
): Promise<AssetJsonResult<T>> {
  const text = await response.text();
  if (expectedSha256) {
    const actual = await sha256Hex(text);
    if (actual !== expectedSha256) {
      throw new AssetFetchError(`asset integrity mismatch: expected ${expectedSha256}, got ${actual}`);
    }
  }
  return {
    value: JSON.parse(text) as T,
    source,
    bytes: new TextEncoder().encode(text).byteLength,
  };
}

function defaultFetch(url: string): Promise<Response> {
  return fetch(url, { credentials: 'same-origin' });
}

function joinBundleUrl(baseUrl: string, relativePath: string): string {
  const cleanRelative = relativePath.replace(/^\/+/, '');
  if (cleanRelative.split('/').includes('..')) {
    throw new AssetFetchError(`asset path escapes bundle: ${relativePath}`);
  }
  return new URL(cleanRelative, baseUrl).toString();
}

function normalizeBaseUrl(baseUrl: string): string {
  return new URL(ensureTrailingSlash(baseUrl), documentOrigin()).toString();
}

function documentOrigin(): string {
  return globalThis.location?.href ?? 'http://localhost/';
}

function ensureTrailingSlash(value: string): string {
  return value.endsWith('/') ? value : `${value}/`;
}

function cacheStorageAvailable(): boolean {
  return typeof caches !== 'undefined' && typeof caches.open === 'function';
}

async function sha256Hex(text: string): Promise<string> {
  if (!globalThis.crypto?.subtle) {
    throw new AssetFetchError('crypto.subtle is unavailable for asset integrity checks');
  }
  const bytes = new TextEncoder().encode(text);
  const hash = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(hash), (byte) => byte.toString(16).padStart(2, '0')).join('');
}

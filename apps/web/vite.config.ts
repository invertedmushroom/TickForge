import { createReadStream, existsSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig, type Plugin } from 'vite';

const webRoot = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(webRoot, '../..');
const webContractRoot = path.resolve(repoRoot, 'target/web-contract');

/**
 * Asset kinds emitted into `target/web-contract/<kind>/`. The dev middleware
 * exposes them at `/__dive_assets__/<kind>/...` to mimic the production CDN
 * layout (`<VITE_ASSET_BASE_URL>/<kind>/...`).
 *
 * Add a new entry here when xtask starts emitting a new asset kind.
 */
const ASSET_KINDS = ['map-bundles'] as const;

const MIME_BY_EXT: Record<string, string> = {
  '.json': 'application/json; charset=utf-8',
  '.glb': 'model/gltf-binary',
  '.gltf': 'model/gltf+json',
  '.bin': 'application/octet-stream',
  '.ktx2': 'image/ktx2',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.webp': 'image/webp',
  '.basis': 'application/octet-stream',
  '.wasm': 'application/wasm',
};

export default defineConfig({
  plugins: [webContractAssetPlugin()],
  resolve: {
    preserveSymlinks: true,
  },
  server: {
    port: 5173,
    strictPort: false,
  },
  build: {
    target: 'es2022',
    sourcemap: true,
    chunkSizeWarningLimit: 2500,
    rolldownOptions: {
      output: {
        codeSplitting: {
          minSize: 20_000,
          groups: [
            {
              name: 'three-vendor',
              test: /node_modules[\\/]three[\\/]/,
              priority: 30,
            },
            {
              name: 'rapier-vendor',
              test: /node_modules[\\/]@dimforge[\\/]rapier3d-compat[\\/]/,
              priority: 30,
            },
            {
              name: 'vendor',
              test: /node_modules[\\/]/,
              priority: 10,
            },
          ],
        },
      },
    },
  },
});

function webContractAssetPlugin(): Plugin {
  return {
    name: 'dive-web-contract-assets',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/__dive_assets__', (req, res, next) => {
        const pathname = decodeURIComponent(new URL(req.url ?? '/', 'http://localhost').pathname);
        const segments = pathname.replace(/^\/+/, '').split('/').filter(Boolean);
        const [kind, ...rest] = segments;
        if (!kind || !ASSET_KINDS.includes(kind as (typeof ASSET_KINDS)[number]) || rest.length === 0) {
          next();
          return;
        }
        const kindRoot = path.resolve(webContractRoot, kind);
        const target = path.resolve(kindRoot, ...rest);
        if (!isInside(kindRoot, target) || !existsSync(target) || !statSync(target).isFile()) {
          next();
          return;
        }
        const mime = MIME_BY_EXT[path.extname(target).toLowerCase()];
        if (mime) {
          res.setHeader('Content-Type', mime);
        }
        res.setHeader('Cache-Control', 'no-store');
        createReadStream(target).pipe(res);
      });
    },
  };
}

function isInside(root: string, target: string): boolean {
  const relative = path.relative(root, target);
  return relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative));
}

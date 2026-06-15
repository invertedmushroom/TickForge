import RAPIER from '@dimforge/rapier3d-compat';

import type { BundleCollider, LoadedMapBundle } from './content';
import { heightfieldToTriMesh } from './heightfield';

export type StaticPhysicsWorld = {
  world: RAPIER.World;
  colliderCount: number;
  step: () => void;
  destroy: () => void;
};

let rapierReady: Promise<void> | undefined;

export function initRapier(): Promise<void> {
  rapierReady ??= RAPIER.init();
  return rapierReady;
}

export async function buildStaticPhysicsWorld(bundle: LoadedMapBundle): Promise<StaticPhysicsWorld> {
  await initRapier();

  const world = new RAPIER.World({ x: 0, y: -9.81, z: 0 });
  for (const collider of bundle.colliders.colliders) {
    const desc = placeCollider(colliderDescFor(collider), collider);
    try {
      world.createCollider(desc);
    } catch (error) {
      console.warn(`static collider ${collider.collider_id} failed to build; using flat filler`, error);
      world.createCollider(placeCollider(fallbackColliderDesc(), collider));
    }
  }

  let colliderCount = bundle.colliders.colliders.length;
  const terrain = bundle.terrainCollision;
  if (terrain && isValidTriMesh(terrain.vertices, terrain.indices)) {
    try {
      world.createCollider(RAPIER.ColliderDesc.trimesh(terrain.vertices, terrain.indices));
      colliderCount += 1;
    } catch (error) {
      console.warn('terrain collision mesh failed to build; prediction uses RON colliders only', error);
    }
  }

  return {
    world,
    colliderCount,
    step: () => world.step(),
    destroy: () => world.free(),
  };
}

function colliderDescFor(collider: BundleCollider): RAPIER.ColliderDesc {
  const shape = collider.shape;
  switch (shape.kind) {
    case 'cuboid':
      return RAPIER.ColliderDesc.cuboid(shape.half_x, shape.half_y, shape.half_z);
    case 'cylinder':
      return RAPIER.ColliderDesc.cylinder(shape.half_height, shape.radius);
    case 'heightfield':
      const terrain = heightfieldToTriMesh(shape);
      if (!terrain) {
        return fallbackColliderDesc();
      }
      return RAPIER.ColliderDesc.trimesh(new Float32Array(terrain.vertices), new Uint32Array(terrain.indices));
    case 'tri_mesh':
      if (!isValidTriMesh(shape.vertices, shape.indices)) {
        return fallbackColliderDesc();
      }
      return RAPIER.ColliderDesc.trimesh(new Float32Array(shape.vertices), new Uint32Array(shape.indices));
    default: {
      const _exhaustive: never = shape;
      throw new Error(`unsupported collider shape: ${JSON.stringify(_exhaustive)}`);
    }
  }
}

function placeCollider(desc: RAPIER.ColliderDesc, collider: BundleCollider): RAPIER.ColliderDesc {
  desc.setTranslation(collider.position[0] ?? 0, collider.position[1] ?? 0, collider.position[2] ?? 0);
  const [x = 0, y = 0, z = 0, w = 1] = collider.rotation;
  desc.setRotation({ x, y, z, w });
  return desc;
}

function fallbackColliderDesc(): RAPIER.ColliderDesc {
  return RAPIER.ColliderDesc.cuboid(0.5, 0.05, 0.5);
}

function isValidTriMesh(vertices: ArrayLike<number>, indices: ArrayLike<number>): boolean {
  if (vertices.length % 3 !== 0 || indices.length % 3 !== 0) {
    return false;
  }
  const vertexCount = vertices.length / 3;
  if (vertexCount < 3 || indices.length < 3) {
    return false;
  }
  for (let i = 0; i < indices.length; i += 1) {
    const index = indices[i];
    if (index < 0 || index >= vertexCount) {
      return false;
    }
  }
  return true;
}

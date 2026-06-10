import * as THREE from 'three';

import { disposeMesh, type ActiveEffect } from './types';

export function spawnTeleportRing(
  group: THREE.Group,
  effects: ActiveEffect[],
  entityId: bigint,
  position: THREE.Vector3,
  colorHex: number,
): void {
  const geometry = new THREE.TorusGeometry(0.65, 0.08, 8, 36);
  geometry.rotateX(Math.PI / 2);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 4.0,
    transparent: true,
    opacity: 0.75,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.set(position.x, position.y + 0.08, position.z);
  group.add(mesh);

  let life = 1.0;
  effects.push({
    executionId: -entityId,
    dispose: () => disposeMesh(mesh),
    update: (delta) => {
      life -= delta * 2.2;
      if (life <= 0) {
        disposeMesh(mesh);
        return false;
      }
      mesh.scale.setScalar(1.0 + (1.0 - life) * 0.8);
      material.opacity = life * 0.75;
      return true;
    },
  });
}

import * as THREE from 'three';

import { disposeMesh, type ActiveEffect } from './types';

export function spawnSphereAt(
  group: THREE.Group,
  effects: ActiveEffect[],
  executionId: bigint,
  position: THREE.Vector3,
  radius: number,
  colorHex: number,
  lifetimeSeconds: number,
): void {
  const geometry = new THREE.SphereGeometry(radius, 20, 14);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 3.0,
    transparent: true,
    opacity: 0.35,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.copy(position);
  group.add(mesh);

  let elapsed = 0;
  const duration = Math.max(0.2, lifetimeSeconds);
  const dispose = () => disposeMesh(mesh);
  effects.push({
    executionId,
    dispose,
    update: (delta) => {
      elapsed += delta;
      const t = elapsed / duration;
      if (t >= 1) {
        dispose();
        return false;
      }
      material.opacity = 0.35 * (1 - t);
      material.emissiveIntensity = 3.0 * (1 - t * 0.5);
      return true;
    },
  });
}

export function spawnImpactBurst(
  group: THREE.Group,
  effects: ActiveEffect[],
  origin: THREE.Vector3,
  colorHex: number,
): void {
  const geometry = new THREE.SphereGeometry(0.45, 16, 12);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 5.0,
    transparent: true,
    opacity: 0.9,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.copy(origin);
  group.add(mesh);

  let life = 1.0;
  effects.push({
    dispose: () => disposeMesh(mesh),
    update: (delta) => {
      life -= delta * 2.5;
      if (life <= 0) {
        disposeMesh(mesh);
        return false;
      }
      mesh.scale.setScalar(1.0 + (1.0 - life) * 1.8);
      material.opacity = life * 0.9;
      return true;
    },
  });
}

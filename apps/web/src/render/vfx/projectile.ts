import * as THREE from 'three';

import { disposeMesh, type ActiveEffect } from './types';

export function spawnProjectileByDirection(
  group: THREE.Group,
  effects: ActiveEffect[],
  executionId: bigint,
  origin: THREE.Vector3,
  direction: THREE.Vector3,
  colorHex: number,
  speed: number,
  maxRange: number,
): void {
  const geometry = new THREE.SphereGeometry(0.3, 14, 14);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 6.0,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.copy(origin);
  group.add(mesh);

  const dir = direction.clone();
  if (dir.lengthSq() < 1e-6) {
    dir.set(0, 0, 1);
  }
  dir.normalize();
  let remainingRange = Math.max(0.001, maxRange);
  const dispose = () => disposeMesh(mesh);
  effects.push({
    executionId,
    dispose,
    update: (delta) => {
      const step = Math.min(speed * delta, remainingRange);
      mesh.position.addScaledVector(dir, step);
      remainingRange -= step;
      if (remainingRange <= 0) {
        dispose();
        return false;
      }
      return true;
    },
  });
}

export function spawnProjectileToTarget(
  group: THREE.Group,
  effects: ActiveEffect[],
  origin: THREE.Vector3,
  target: THREE.Vector3,
  colorHex: number,
  speed: number,
  onImpact: (position: THREE.Vector3, color: number) => void,
): void {
  const geometry = new THREE.SphereGeometry(0.3, 14, 14);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 6.0,
  });
  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.copy(origin);
  group.add(mesh);

  const direction = new THREE.Vector3().subVectors(target, origin);
  const totalDistance = direction.length();
  if (totalDistance < 1e-3) {
    onImpact(origin, colorHex);
    disposeMesh(mesh);
    return;
  }
  direction.normalize();
  let travelled = 0;
  effects.push({
    dispose: () => disposeMesh(mesh),
    update: (delta) => {
      const step = speed * delta;
      mesh.position.addScaledVector(direction, step);
      travelled += step;
      if (travelled >= totalDistance) {
        onImpact(mesh.position, colorHex);
        disposeMesh(mesh);
        return false;
      }
      return true;
    },
  });
}

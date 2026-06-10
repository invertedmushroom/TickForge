import * as THREE from 'three';

import { disposeMesh, type ActiveEffect, type CasterAttachment } from './types';

export function spawnCapsuleFront(
  group: THREE.Group,
  effects: ActiveEffect[],
  radius: number,
  halfHeight: number,
  colorHex: number,
  lifetime: number,
  caster: CasterAttachment | undefined,
  fallbackOrigin: THREE.Vector3,
): void {
  const geometry = new THREE.CapsuleGeometry(radius, halfHeight * 2, 6, 12);
  geometry.rotateX(Math.PI / 2);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 4.0,
    transparent: true,
    opacity: 0.7,
  });
  const mesh = new THREE.Mesh(geometry, material);
  group.add(mesh);

  const forwardOffset = halfHeight + radius * 0.5;
  const lifetimeSec = Math.max(0.2, lifetime);
  let elapsed = 0;
  effects.push({
    dispose: () => disposeMesh(mesh),
    update: (delta) => {
      elapsed += delta;
      const t = elapsed / lifetimeSec;
      if (t >= 1) {
        disposeMesh(mesh);
        return false;
      }
      const anchor = caster?.position() ?? fallbackOrigin;
      const rotation = caster?.rotation() ?? new THREE.Quaternion();
      const forward = new THREE.Vector3(0, 0, forwardOffset).applyQuaternion(rotation);
      mesh.position.set(anchor.x + forward.x, anchor.y + 1.0, anchor.z + forward.z);
      mesh.quaternion.copy(rotation);
      material.opacity = 0.7 * (1 - t);
      material.emissiveIntensity = 4.0 * (1 - t * 0.5);
      return true;
    },
  });
}

export function spawnSphereAround(
  group: THREE.Group,
  effects: ActiveEffect[],
  radius: number,
  colorHex: number,
  lifetime: number,
  pulseSeconds: number,
  caster: CasterAttachment | undefined,
  fallbackOrigin: THREE.Vector3,
): void {
  const geometry = new THREE.SphereGeometry(radius, 24, 16);
  const material = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 3.0,
    transparent: true,
    opacity: 0.35,
  });
  const mesh = new THREE.Mesh(geometry, material);
  group.add(mesh);

  const lifetimeSec = Math.max(0.4, lifetime);
  const pulseWindow = pulseSeconds > 0 ? Math.min(pulseSeconds, 0.3) : 0;
  let elapsed = 0;
  let pulseCooldown = pulseSeconds > 0 ? pulseSeconds : 0;
  effects.push({
    dispose: () => disposeMesh(mesh),
    update: (delta) => {
      elapsed += delta;
      const t = elapsed / lifetimeSec;
      if (t >= 1) {
        disposeMesh(mesh);
        return false;
      }
      const anchor = caster?.position() ?? fallbackOrigin;
      mesh.position.set(anchor.x, anchor.y + 1.0, anchor.z);

      let pulseStrength = 0;
      if (pulseSeconds > 0) {
        pulseCooldown -= delta;
        while (pulseCooldown <= 0) pulseCooldown += pulseSeconds;
        if (pulseCooldown >= pulseSeconds - pulseWindow) {
          pulseStrength = (pulseCooldown - (pulseSeconds - pulseWindow)) / pulseWindow;
        }
      }
      const fade = 1 - t;
      material.opacity = (0.25 + 0.35 * pulseStrength) * fade;
      material.emissiveIntensity = (2.5 + 5.0 * pulseStrength) * fade;
      return true;
    },
  });
}

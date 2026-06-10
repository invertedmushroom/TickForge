import * as THREE from 'three';

import { disposeMesh, type ActiveEffect } from './types';

export function spawnGroundTorus(
  group: THREE.Group,
  effects: ActiveEffect[],
  radius: number,
  colorHex: number,
  lifetime: number,
  pulseSeconds: number,
  centre: THREE.Vector3,
  executionId?: bigint,
): void {
  const ringGeom = new THREE.TorusGeometry(radius, Math.max(0.08, radius * 0.08), 10, 48);
  ringGeom.rotateX(Math.PI / 2);
  const ringMat = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 6.0,
    transparent: true,
    opacity: 0.85,
  });
  const ring = new THREE.Mesh(ringGeom, ringMat);
  ring.position.set(centre.x, centre.y + 0.05, centre.z);
  group.add(ring);

  const discGeom = new THREE.CircleGeometry(radius * 0.95, 48);
  discGeom.rotateX(-Math.PI / 2);
  const discMat = new THREE.MeshStandardMaterial({
    color: colorHex,
    emissive: colorHex,
    emissiveIntensity: 1.0,
    transparent: true,
    opacity: 0.18,
    side: THREE.DoubleSide,
  });
  const disc = new THREE.Mesh(discGeom, discMat);
  disc.position.set(centre.x, centre.y + 0.04, centre.z);
  group.add(disc);

  const lifetimeSec = Math.max(0.6, lifetime);
  const pulseWindow = pulseSeconds > 0 ? Math.min(pulseSeconds, 0.25) : 0;
  let elapsed = 0;
  let pulseCooldown = pulseSeconds > 0 ? pulseSeconds : 0;
  const dispose = () => {
    disposeMesh(ring);
    disposeMesh(disc);
  };
  effects.push({
    executionId,
    dispose,
    update: (delta) => {
      elapsed += delta;
      const t = elapsed / lifetimeSec;
      if (t >= 1) {
        dispose();
        return false;
      }
      let pulseStrength = 0;
      if (pulseSeconds > 0) {
        pulseCooldown -= delta;
        while (pulseCooldown <= 0) pulseCooldown += pulseSeconds;
        if (pulseCooldown >= pulseSeconds - pulseWindow) {
          pulseStrength = (pulseCooldown - (pulseSeconds - pulseWindow)) / pulseWindow;
        }
      }
      const fade = 1 - t;
      ringMat.opacity = (0.7 + 0.3 * pulseStrength) * fade;
      ringMat.emissiveIntensity = (5.0 + 6.0 * pulseStrength) * fade;
      discMat.opacity = (0.14 + 0.18 * pulseStrength) * fade;
      return true;
    },
  });
}

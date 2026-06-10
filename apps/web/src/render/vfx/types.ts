import type * as THREE from 'three';

export type ActiveEffect = {
  executionId?: bigint;
  update: (delta: number) => boolean;
  dispose?: () => void;
};

export type CasterAttachment = {
  position(): THREE.Vector3 | undefined;
  rotation(): THREE.Quaternion | undefined;
};

export function disposeMesh(mesh: THREE.Mesh): void {
  mesh.parent?.remove(mesh);
  mesh.geometry?.dispose();
  const material = mesh.material as THREE.Material | THREE.Material[] | undefined;
  if (Array.isArray(material)) {
    for (const item of material) item.dispose();
  } else {
    material?.dispose();
  }
}

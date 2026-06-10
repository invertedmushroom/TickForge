import * as THREE from 'three';

const MATERIAL_TEXTURE_PROPERTIES = [
  'map',
  'alphaMap',
  'aoMap',
  'bumpMap',
  'clearcoatNormalMap',
  'clearcoatRoughnessMap',
  'displacementMap',
  'emissiveMap',
  'envMap',
  'gradientMap',
  'lightMap',
  'matcap',
  'metalnessMap',
  'normalMap',
  'opacityMap',
  'roughnessMap',
  'specularMap',
  'sheenColorMap',
  'sheenRoughnessMap',
  'transmissionMap',
  'thicknessMap',
] as const;

export function disposeThreeObject(object: THREE.Object3D): void {
  object.traverse((node) => {
    const geometry = (node as { geometry?: { dispose(): void } }).geometry;
    if (geometry) {
      geometry.dispose();
    }

    const material = (node as { material?: THREE.Material | THREE.Material[] }).material;
    if (material) {
      disposeMaterial(material);
    }
  });
}

function disposeMaterial(material: THREE.Material | THREE.Material[]): void {
  if (Array.isArray(material)) {
    material.forEach(disposeMaterial);
    return;
  }

  disposeMaterialTextures(material);
  material.dispose();
}

function disposeMaterialTextures(material: THREE.Material): void {
  const mat = material as unknown as Record<string, unknown>;
  for (const key of MATERIAL_TEXTURE_PROPERTIES) {
    const texture = mat[key] as unknown;
    if (texture instanceof THREE.Texture) {
      texture.dispose();
    }
  }
}

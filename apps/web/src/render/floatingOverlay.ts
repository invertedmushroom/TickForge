import * as THREE from 'three';

export type FloatingOverlayTone = 'damage' | 'heal';

export type FloatingOverlaySpawnOptions = {
  position: THREE.Vector3;
  amount: number;
  tone: FloatingOverlayTone;
};

type FloatingOverlayEntry = {
  id: number;
  element: HTMLDivElement;
  position: THREE.Vector3;
  ageSeconds: number;
  durationSeconds: number;
  liftPixels: number;
  lateralPixels: number;
};

const DEFAULT_DURATION_SECONDS = 1.05;
const DEFAULT_LIFT_PIXELS = 42;

export class FloatingOverlayLayer {
  readonly root: HTMLDivElement;
  private readonly entries: FloatingOverlayEntry[] = [];
  private nextId = 1;

  constructor(parent: HTMLElement) {
    this.root = document.createElement('div');
    this.root.className = 'floating-overlay-layer';
    this.root.setAttribute('aria-hidden', 'true');
    parent.append(this.root);
  }

  spawnDamage(position: THREE.Vector3, amount: number, tone: FloatingOverlayTone = 'damage'): void {
    this.spawn({ position, amount, tone });
  }

  spawn(options: FloatingOverlaySpawnOptions): void {
    const id = this.nextId++;
    const element = document.createElement('div');
    element.className = `floating-overlay floating-overlay--${options.tone}`;
    element.textContent = formatAmount(options.amount, options.tone);
    this.root.append(element);

    this.entries.push({
      id,
      element,
      position: options.position.clone(),
      ageSeconds: 0,
      durationSeconds: DEFAULT_DURATION_SECONDS,
      liftPixels: DEFAULT_LIFT_PIXELS,
      lateralPixels: lateralOffsetFor(id),
    });
  }

  update(camera: THREE.Camera, viewport: DOMRect, deltaSeconds: number): void {
    const nextEntries: FloatingOverlayEntry[] = [];
    for (const entry of this.entries) {
      entry.ageSeconds += deltaSeconds;
      const progress = Math.min(1, entry.ageSeconds / entry.durationSeconds);
      if (progress >= 1) {
        entry.element.remove();
        continue;
      }

      const screen = projectToViewport(entry.position, camera, viewport);
      if (!screen.visible) {
        entry.element.style.opacity = '0';
        nextEntries.push(entry);
        continue;
      }

      const eased = easeOutCubic(progress);
      const lift = entry.liftPixels * eased;
      const scale = 1 + 0.22 * (1 - Math.abs(progress - 0.22) / 0.78);
      const opacity = progress < 0.72 ? 1 : Math.max(0, 1 - (progress - 0.72) / 0.28);
      entry.element.style.opacity = opacity.toFixed(3);
      entry.element.style.transform = `translate3d(${(screen.x + entry.lateralPixels).toFixed(1)}px, ${(screen.y - lift).toFixed(1)}px, 0) translate(-50%, -100%) scale(${scale.toFixed(3)})`;
      nextEntries.push(entry);
    }

    this.entries.length = 0;
    this.entries.push(...nextEntries);
  }

  activeCount(): number {
    return this.entries.length;
  }

  destroy(): void {
    for (const entry of this.entries) {
      entry.element.remove();
    }
    this.entries.length = 0;
    this.root.remove();
  }
}

function projectToViewport(
  position: THREE.Vector3,
  camera: THREE.Camera,
  viewport: DOMRect,
): { visible: boolean; x: number; y: number } {
  const projected = position.clone().project(camera);
  const visible = projected.z >= -1 && projected.z <= 1;
  return {
    visible,
    x: (projected.x * 0.5 + 0.5) * viewport.width,
    y: (-projected.y * 0.5 + 0.5) * viewport.height,
  };
}

function formatAmount(amount: number, tone: FloatingOverlayTone): string {
  const rounded = Math.max(0, Math.round(amount));
  return tone === 'heal' ? `+${rounded}` : `${rounded}`;
}

function lateralOffsetFor(id: number): number {
  return ((id * 37) % 23) - 11;
}

function easeOutCubic(value: number): number {
  return 1 - Math.pow(1 - value, 3);
}

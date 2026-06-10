import * as THREE from 'three';
import { disposeThreeObject } from '../utils/three';
import { SECONDS_PER_TICK, SIM_TICKS_PER_SECOND } from '../timing';

import type { EntityHealth, EntityTransform, NearbyEntity } from '../stdb/connection';
import { CharacterModel, CharacterModelLoader, type CharacterOverlayAnimationRequest } from '../render/characterModel';

export const REMOTE_BUFFER_TICKS = 2.0;
export const MAX_EXTRAPOLATION_SECS = 0.15;
export const MAX_SNAPSHOT_HISTORY = 12;
export const INTERPOLATION_TELEPORT_THRESHOLD = 15.0;

export { SIM_TICKS_PER_SECOND };

type Snapshot = {
  tick: bigint;
  posX: number;
  posY: number;
  posZ: number;
  rotX: number;
  rotY: number;
  rotZ: number;
  rotW: number;
};

export type RemoteRecord = {
  entityId: bigint;
  kind?: NearbyEntity['kind'];
  snapshots: Snapshot[];
  lastObservedAtMs: number;
  mesh: THREE.Group;
  fallbackMesh?: THREE.Mesh;
  characterModel?: CharacterModel;
  /**
   * Skill animation queued before the character GLB finished loading. We
   * flush it once the model attaches so a remote spider that casts the
   * instant its proxy is created still plays the swing.
   */
  pendingSkillAnimation?: CharacterOverlayAnimationRequest;
  isExtrapolating: boolean;
};

export type RemoteWorldStats = {
  entityCount: number;
  extrapolationEvents: number;
  extrapolationSecsCurrent: number;
  extrapolationSecsMax: number;
  snapshotGapEwma: number;
  snapshotGapMax: number;
};

export class RemoteEntityWorld {
  private readonly records = new Map<bigint, RemoteRecord>();
  private readonly group: THREE.Group;
  private extrapolationEvents = 0;
  private extrapolationSecsCurrent = 0;
  private extrapolationSecsMax = 0;
  private snapshotGapEwma = 0;
  private snapshotGapMax = 0;
  private highlightSelected: bigint | undefined;
  private highlightHover: bigint | undefined;

  constructor(parent: THREE.Object3D, private characterLoader: CharacterModelLoader) {
    this.group = new THREE.Group();
    this.group.name = 'remote-entities';
    parent.add(this.group);
  }

  getRecord(entityId: bigint): RemoteRecord | undefined {
    return this.records.get(entityId);
  }

  /** Update snapshot buffers from the latest server view. */
  ingest(
    transforms: readonly EntityTransform[],
    entities: ReadonlyMap<bigint, NearbyEntity>,
    nowMs: number,
    healthByEntity: ReadonlyMap<bigint, EntityHealth> = new Map(),
  ): void {
    for (const transform of transforms) {
      const kind = entities.get(transform.entityId)?.kind;
      const health = healthByEntity.get(transform.entityId);
      let record = this.records.get(transform.entityId);
      if (!record) {
        record = {
          entityId: transform.entityId,
          kind,
          snapshots: [],
          lastObservedAtMs: nowMs,
          mesh: new THREE.Group(),
          isExtrapolating: false,
        };
        const fallback = this.buildFallbackMesh(kind);
        record.fallbackMesh = fallback;
        record.mesh.add(fallback);

        const rec = record;
        void this.characterLoader.load().then((gltfGroup) => {
          if (!this.records.has(transform.entityId)) return;
          if (rec.fallbackMesh) {
            rec.mesh.remove(rec.fallbackMesh);
            disposeThreeObject(rec.fallbackMesh);
            rec.fallbackMesh = undefined;
          }
          rec.characterModel = new CharacterModel(gltfGroup);
          rec.mesh.add(rec.characterModel.group);
          if (rec.kind) {
            tintMesh(rec.mesh, colorForKind(rec.kind));
          }
          if (rec.pendingSkillAnimation !== undefined) {
            rec.characterModel.playOverlayAnimation(rec.pendingSkillAnimation);
            rec.pendingSkillAnimation = undefined;
          }
        }).catch((err) => console.warn('Failed to load remote character model', err));

        this.group.add(record.mesh);
        this.records.set(transform.entityId, record);
      } else if (kind && record.kind?.tag !== kind.tag) {
        record.kind = kind;
        tintMesh(record.mesh, colorForKind(kind));
      }
      updateHealthPresentation(record.mesh, transform.entityId, health);

      let last: Snapshot | undefined = record.snapshots[record.snapshots.length - 1];
      if (last) {
        const dx = transform.posX - last.posX;
        const dy = transform.posY - last.posY;
        const dz = transform.posZ - last.posZ;
        const distSq = dx * dx + dy * dy + dz * dz;
        if (distSq > INTERPOLATION_TELEPORT_THRESHOLD * INTERPOLATION_TELEPORT_THRESHOLD) {
          record.snapshots = [];
          last = undefined;
        }
      }

      const shouldAppend = !last || transform.lastTick > last.tick;
      if (shouldAppend) {
        if (last) {
          const deltaTicks = transform.lastTick - last.tick;
          if (deltaTicks > 0n) {
            const gap = Number(deltaTicks) * SECONDS_PER_TICK;
            if (Number.isFinite(gap) && gap > 0) {
              this.snapshotGapEwma = this.snapshotGapEwma * 0.9 + gap * 0.1;
              this.snapshotGapMax = Math.max(this.snapshotGapMax, gap);
            }
          }
        }
        record.snapshots.push({
          tick: transform.lastTick,
          posX: transform.posX,
          posY: transform.posY,
          posZ: transform.posZ,
          rotX: transform.rotX,
          rotY: transform.rotY,
          rotZ: transform.rotZ,
          rotW: transform.rotW,
        });
        while (record.snapshots.length > MAX_SNAPSHOT_HISTORY) {
          record.snapshots.shift();
        }
      }
      record.lastObservedAtMs = nowMs;
    }

    // Prune entities no longer in AOI.
    const live = new Set<bigint>();
    for (const transform of transforms) {
      live.add(transform.entityId);
    }
    for (const [entityId, record] of this.records) {
      if (!live.has(entityId)) {
        this.group.remove(record.mesh);
        disposeThreeObject(record.mesh);
        this.records.delete(entityId);
      }
    }
  }

  /** Render-time interpolation. `latestServerTick` is the most recent observed tick id. */
  step(latestServerTick: bigint | undefined, deltaSeconds: number): void {
    if (latestServerTick === undefined) {
      return;
    }

    const renderTick = Number(latestServerTick) - REMOTE_BUFFER_TICKS;
    let extrapolationSecsCurrent = 0;

    for (const record of this.records.values()) {
      const snapshots = record.snapshots;
      if (snapshots.length === 0) {
        continue;
      }

      const newest = snapshots[snapshots.length - 1]!;
      const oldest = snapshots[0]!;

      if (snapshots.length === 1 || renderTick <= Number(oldest.tick)) {
        record.isExtrapolating = false;
        applyToMesh(record.mesh, oldest);
        continue;
      }

      if (renderTick >= Number(newest.tick)) {
        // Extrapolate briefly when buffer runs dry.
        const overshootTicks = renderTick - Number(newest.tick);
        const overshootSecs = Math.min(overshootTicks * SECONDS_PER_TICK, MAX_EXTRAPOLATION_SECS);
        if (overshootSecs > 0) {
          if (!record.isExtrapolating) {
            this.extrapolationEvents += 1;
          }
          record.isExtrapolating = true;
          extrapolationSecsCurrent = Math.max(extrapolationSecsCurrent, overshootSecs);
        } else {
          record.isExtrapolating = false;
        }
        const prev = snapshots[snapshots.length - 2] ?? newest;
        applyExtrapolated(record.mesh, prev, newest, overshootSecs);
        continue;
      }

      record.isExtrapolating = false;

      // Locate bracketing snapshots.
      let before = snapshots[0]!;
      let after = snapshots[1]!;
      for (let idx = 1; idx < snapshots.length; idx += 1) {
        const candidate = snapshots[idx]!;
        if (Number(candidate.tick) >= renderTick) {
          after = candidate;
          before = snapshots[idx - 1]!;
          break;
        }
      }

      const span = Math.max(1, Number(after.tick - before.tick));
      const t = (renderTick - Number(before.tick)) / span;
      applyInterpolated(record.mesh, before, after, t);
    }

    for (const record of this.records.values()) {
      if (record.characterModel) {
        // Calculate speed based on movement per frame
        const dx = record.mesh.position.x - ((record as any)._lastX ?? record.mesh.position.x);
        const dy = record.mesh.position.y - ((record as any)._lastY ?? record.mesh.position.y);
        const dz = record.mesh.position.z - ((record as any)._lastZ ?? record.mesh.position.z);
        const speed = deltaSeconds > 0 ? Math.hypot(dx, dz) / deltaSeconds : 0;
        const verticalSpeed = deltaSeconds > 0 ? dy / deltaSeconds : 0;

        if (verticalSpeed > 4.0) { // arbitrary threshold for sudden upward movement
          record.characterModel.playJumpAnimation();
        }

        (record as any)._lastX = record.mesh.position.x;
        (record as any)._lastY = record.mesh.position.y;
        (record as any)._lastZ = record.mesh.position.z;

        record.characterModel.update(deltaSeconds, speed);
      }
    }

    this.extrapolationSecsCurrent = extrapolationSecsCurrent;
    this.extrapolationSecsMax = Math.max(this.extrapolationSecsMax, extrapolationSecsCurrent);
  }

  stats(): RemoteWorldStats {
    return {
      entityCount: this.records.size,
      extrapolationEvents: this.extrapolationEvents,
      extrapolationSecsCurrent: this.extrapolationSecsCurrent,
      extrapolationSecsMax: this.extrapolationSecsMax,
      snapshotGapEwma: this.snapshotGapEwma,
      snapshotGapMax: this.snapshotGapMax,
    };
  }

  destroy(): void {
    for (const record of this.records.values()) {
      this.group.remove(record.mesh);
      disposeThreeObject(record.mesh);
    }
    this.records.clear();
    this.group.parent?.remove(this.group);
  }

  /** World-space position of the entity's mesh, or undefined if not present. */
  getPosition(entityId: bigint): { x: number; y: number; z: number } | undefined {
    const record = this.records.get(entityId);
    if (!record) {
      return undefined;
    }
    const p = record.mesh.position;
    return { x: p.x, y: p.y, z: p.z };
  }

  /**
   * Play (or queue) the skill animation for a remote entity. If the GLB
   * hasn't finished loading yet the request is stored on the record and
   * flushed when the character model attaches, so a CastStart arriving in
   * the same snapshot that introduces the entity isn't dropped silently.
   */
  triggerSkillAnimation(entityId: bigint, request: CharacterOverlayAnimationRequest): void {
    const record = this.records.get(entityId);
    if (!record) return;
    if (record.characterModel) {
      record.characterModel.playOverlayAnimation(request);
    } else {
      record.pendingSkillAnimation = request;
    }
  }

  /** Cancel any in-flight or pending skill animation for a remote entity. */
  cancelSkillAnimation(entityId: bigint): void {
    const record = this.records.get(entityId);
    if (!record) return;
    record.pendingSkillAnimation = undefined;
    record.characterModel?.cancelSkillAnimation();
  }

  /** Returns the set of entity ids currently tracked (one entry per AOI member). */
  liveIds(): Set<bigint> {
    return new Set(this.records.keys());
  }

  /**
   * Apply selection/hover highlight. Pass undefined to clear.
   * Selected outline is brighter than hover; selected wins ties.
   */
  setHighlight(selected: bigint | undefined, hover: bigint | undefined): void {
    if (selected === this.highlightSelected && hover === this.highlightHover) {
      return;
    }
    // Reset previously highlighted entries that are no longer highlighted.
    if (this.highlightSelected !== undefined && this.highlightSelected !== selected && this.highlightSelected !== hover) {
      this.applyHighlightTo(this.highlightSelected, 'none');
    }
    if (
      this.highlightHover !== undefined &&
      this.highlightHover !== hover &&
      this.highlightHover !== selected &&
      this.highlightHover !== this.highlightSelected
    ) {
      this.applyHighlightTo(this.highlightHover, 'none');
    }
    this.highlightSelected = selected;
    this.highlightHover = hover;
    if (hover !== undefined && hover !== selected) {
      this.applyHighlightTo(hover, 'hover');
    }
    if (selected !== undefined) {
      this.applyHighlightTo(selected, 'selected');
    }
  }

  private applyHighlightTo(entityId: bigint, mode: 'selected' | 'hover' | 'none'): void {
    const record = this.records.get(entityId);
    if (!record) {
      return;
    }
    
    let emissiveHex = 0x000000;
    let emissiveIntensity = 0;
    
    switch (mode) {
      case 'selected':
        emissiveHex = 0xffd166;
        emissiveIntensity = 0.85;
        break;
      case 'hover':
        emissiveHex = 0x88c0ff;
        emissiveIntensity = 0.45;
        break;
      case 'none':
        break;
    }

    record.mesh.traverse((node) => {
      if (node instanceof THREE.Mesh) {
        const mats = Array.isArray(node.material) ? node.material : [node.material];
        for (const material of mats) {
          if (material instanceof THREE.MeshStandardMaterial) {
            material.emissive.setHex(emissiveHex);
            material.emissiveIntensity = emissiveIntensity;
          }
        }
      }
    });
  }

  private buildFallbackMesh(kind: NearbyEntity['kind'] | undefined): THREE.Mesh {
    const color = colorForKind(kind);
    const material = new THREE.MeshStandardMaterial({ color, metalness: 0.05, roughness: 0.55 });
    const geometry = new THREE.CapsuleGeometry(0.35, 0.9, 6, 12);
    const mesh = new THREE.Mesh(geometry, material);
    mesh.position.y = 0.9;
    mesh.castShadow = false;
    mesh.receiveShadow = true;
    if (typeof document !== 'undefined') {
      const label = buildNameLabel();
      label.name = 'remote-name-label';
      label.position.set(0, 0.72, 0); // relative to capsule center
      mesh.add(label);
    }
    return mesh;
  }
}

function applyToMesh(mesh: THREE.Object3D, snapshot: Snapshot): void {
  mesh.position.set(snapshot.posX, snapshot.posY - 1.05, snapshot.posZ);
  mesh.quaternion.set(snapshot.rotX, snapshot.rotY, snapshot.rotZ, snapshot.rotW);
}

const _q1 = new THREE.Quaternion();
const _q2 = new THREE.Quaternion();
function applyInterpolated(
  mesh: THREE.Object3D,
  from: Snapshot,
  to: Snapshot,
  t: number,
): void {
  const clamped = Math.max(0, Math.min(1, t));
  mesh.position.set(
    from.posX + (to.posX - from.posX) * clamped,
    from.posY + (to.posY - from.posY) * clamped - 1.05,
    from.posZ + (to.posZ - from.posZ) * clamped,
  );
  _q1.set(from.rotX, from.rotY, from.rotZ, from.rotW);
  _q2.set(to.rotX, to.rotY, to.rotZ, to.rotW);
  _q1.slerp(_q2, clamped);
  mesh.quaternion.copy(_q1);
}

function applyExtrapolated(mesh: THREE.Object3D, prev: Snapshot, newest: Snapshot, overshootSecs: number): void {
  const tickDeltaSecs = Math.max(
    SECONDS_PER_TICK,
    Number(prev.tick < newest.tick ? newest.tick - prev.tick : 1n) * SECONDS_PER_TICK,
  );
  const vx = (newest.posX - prev.posX) / tickDeltaSecs;
  const vy = (newest.posY - prev.posY) / tickDeltaSecs;
  const vz = (newest.posZ - prev.posZ) / tickDeltaSecs;

  // Apply a velocity compression curve so the entity decays smoothly towards a ceiling under severe server delays.
  // Using an asymptotic curve: effectiveSecs = COMPRESSION_TAU * (1.0 - Math.exp(-overshootSecs / COMPRESSION_TAU))
  // At t=0, effectiveSecs = 0 and d(effective)/dt = 1, giving smooth continuity from interpolation.
  const COMPRESSION_TAU = 0.1;
  const effectiveSecs = COMPRESSION_TAU * (1.0 - Math.exp(-overshootSecs / COMPRESSION_TAU));

  mesh.position.set(
    newest.posX + vx * effectiveSecs,
    newest.posY + vy * effectiveSecs - 1.05,
    newest.posZ + vz * effectiveSecs,
  );
  mesh.quaternion.set(newest.rotX, newest.rotY, newest.rotZ, newest.rotW);
}

function colorForKind(kind: NearbyEntity['kind'] | undefined): number {
  if (!kind) {
    return 0x9aa0a6;
  }
  switch (kind.tag) {
    case 'Player':
      return 0x6fd3c8;
    case 'Npc':
      return 0xe05b67;
    case 'Boss':
      return 0xff7a1a;
    case 'Projectile':
      return 0xf0e44d;
    case 'Hazard':
      return 0xff4d4d;
    case 'Prop':
      return 0x8f7c5a;
    default:
      return 0x9aa0a6;
  }
}

function tintMesh(object: THREE.Object3D, color: number): void {
  object.traverse((node) => {
    if (node instanceof THREE.Mesh) {
      const mats = Array.isArray(node.material) ? node.material : [node.material];
      for (const material of mats) {
        if (material instanceof THREE.MeshStandardMaterial) {
          material.color.setHex(color);
        }
      }
    }
  });
}


function updateHealthPresentation(mesh: THREE.Object3D, entityId: bigint, health: EntityHealth | undefined): void {
  let bar = mesh.getObjectByName('remote-health-bar') as THREE.Group | undefined;
  if (!health || health.maxHp <= 0) {
    if (bar) {
      mesh.remove(bar);
      disposeThreeObject(bar);
    }
    updateNameLabel(mesh, '');
    return;
  }

  if (!bar) {
    bar = buildHealthBar();
    bar.name = 'remote-health-bar';
    bar.position.set(0, 1.9, 0); // Put it above the 1.8m character
    mesh.add(bar);
  }

  const ratio = Math.max(0, Math.min(1, health.hp / health.maxHp));
  const fill = bar.getObjectByName('remote-health-fill');
  if (fill) {
    fill.scale.x = ratio;
    fill.position.x = -0.32 + 0.32 * ratio;
  }

  updateNameLabel(mesh, `#${entityId} ${Math.ceil(health.hp)}/${Math.ceil(health.maxHp)}`);
}

function updateNameLabel(mesh: THREE.Object3D, text: string): void {
  const label = mesh.getObjectByName('remote-name-label');
  if (!(label instanceof THREE.Sprite) || label.userData['text'] === text) {
    return;
  }
  label.material.map?.dispose();
  label.material.map = labelTexture(text);
  label.material.needsUpdate = true;
  label.userData['text'] = text;
}

function buildHealthBar(): THREE.Group {
  const group = new THREE.Group();
  const background = new THREE.Mesh(
    new THREE.PlaneGeometry(0.72, 0.08),
    new THREE.MeshBasicMaterial({ color: 0x182226, transparent: true, opacity: 0.9, depthTest: false }),
  );
  const fill = new THREE.Mesh(
    new THREE.PlaneGeometry(0.64, 0.045),
    new THREE.MeshBasicMaterial({ color: 0x65e08d, transparent: true, opacity: 0.95, depthTest: false }),
  );
  fill.name = 'remote-health-fill';
  fill.position.z = 0.002;
  group.add(background, fill);
  return group;
}

function buildNameLabel(): THREE.Sprite {
  const material = new THREE.SpriteMaterial({
    map: labelTexture(''),
    transparent: true,
    depthTest: false,
  });
  const sprite = new THREE.Sprite(material);
  sprite.scale.set(0.9, 0.18, 1);
  return sprite;
}

function labelTexture(text: string): THREE.CanvasTexture {
  const canvas = document.createElement('canvas');
  canvas.width = 256;
  canvas.height = 64;
  const ctx = canvas.getContext('2d');
  if (ctx) {
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    if (text) {
      ctx.fillStyle = 'rgba(5, 10, 12, 0.72)';
      roundRect(ctx, 8, 10, 240, 42, 8);
      ctx.fill();
      ctx.fillStyle = '#eef5f3';
      ctx.font = '24px sans-serif';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(text, 128, 32);
    }
  }
  const texture = new THREE.CanvasTexture(canvas);
  texture.colorSpace = THREE.SRGBColorSpace;
  return texture;
}

function roundRect(ctx: CanvasRenderingContext2D, x: number, y: number, width: number, height: number, radius: number): void {
  ctx.beginPath();
  ctx.moveTo(x + radius, y);
  ctx.lineTo(x + width - radius, y);
  ctx.quadraticCurveTo(x + width, y, x + width, y + radius);
  ctx.lineTo(x + width, y + height - radius);
  ctx.quadraticCurveTo(x + width, y + height, x + width - radius, y + height);
  ctx.lineTo(x + radius, y + height);
  ctx.quadraticCurveTo(x, y + height, x, y + height - radius);
  ctx.lineTo(x, y + radius);
  ctx.quadraticCurveTo(x, y, x + radius, y);
  ctx.closePath();
}

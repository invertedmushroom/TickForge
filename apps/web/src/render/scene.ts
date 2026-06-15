import RAPIER from '@dimforge/rapier3d-compat';
import * as THREE from 'three';

import { disposeThreeObject } from '../utils/three';
import { getStartupMapBundle, type BundleCollider, type LoadedMapBundle } from '../world/content';
import { heightfieldToTriMesh } from '../world/heightfield';
import { buildStaticPhysicsWorld } from '../world/physics';
import type { LoadedVisualMap } from '../world/mapAssets';
import {
  PLAYER_CAPSULE_HALF_HEIGHT,
  PLAYER_CAPSULE_RADIUS,
  PlayerCharacterController,
} from '../world/characterController';
import { ClientPhysicsPrediction } from '../world/clientPrediction';
import { LocalPlayerReconciler } from '../entities/localPlayer';
import { RemoteEntityWorld } from '../entities/remoteEntities';
import { CharacterModelLoader, CharacterModel } from './characterModel';
import { normalizeDirection } from '../utils/math';
import type { StdbClient, StdbSnapshot } from '../stdb/connection';
import type { IntentAction } from '@dive/client-contract/bindings/types';
import { blockIntent, interactIntent, jumpIntent, moveIntent, stopIntent, weaponSwapIntent } from '../net/inputQueue';
import {
  ABILITY_TICK_RATE,
  DEFAULT_ABILITY_SLOTS,
  type AbilityCatalogEntry,
  type AbilitySlot,
  type AbilitySlotBinding,
  type PreviewShape,
} from '../abilities/catalog';
import { abilitySlotFromKeyboardEvent } from '../input/keyBindings';
import { gameplayKeyboardInputAllowed, isUiKeyboardTarget, type InputFocusMode } from '../input/focusGate';
import { resolveInteractTarget } from '../input/interactResolver';
import { OrbitCameraRig, rotateMovementByCameraYaw } from '../input/cameraRig';
import {
  createTouchLookControls,
  createTouchMovementControls,
  type TouchLookControlsHandle,
  type TouchLookVector,
  type TouchMovementControlsHandle,
  type TouchMovementVector,
} from '../input/touchControls';
import { DEFAULT_TOUCH_CONTROL_PREFERENCES, type TouchControlPreferences } from '../input/touchPreferences';
import { deriveAbilityInteractionPolicy, type AbilityInteractionPolicy } from '../abilities/interactionPolicy';
import {
  createActionBar,
  type ActionBarHandle,
  type ActionBarSlotVisualState,
} from '../ui/actionBar';
import {
  buildUseAbilityIntent,
  directionFromAimPoint,
  findSoftTarget,
  releaseAbilityAction,
  softTargetCandidatesFromTransforms,
  type AbilityAimState,
  type AbilityIntentResult,
  type Vec3,
} from '../abilities/intent';
import { TargetSelection, type ScreenProjector } from '../abilities/selection';
import { buildLootHudModel } from '../ui/lootHud';
import { FloatingOverlayLayer } from './floatingOverlay';
import { VfxManager, type CasterAttachment } from './vfx.ts';
import { visualFor } from './skillVisuals';
import { VisualDirector } from './visualDirector';
import { EffectComposer } from 'three/examples/jsm/postprocessing/EffectComposer.js';
import { RenderPass } from 'three/examples/jsm/postprocessing/RenderPass.js';
import { UnrealBloomPass } from 'three/examples/jsm/postprocessing/UnrealBloomPass.js';

export type SceneHandle = {
  destroy: () => void;
};

export type SceneOptions = {
  inputEnabled?: () => boolean;
  inputMode?: () => InputFocusMode;
  subscribeInputMode?: (listener: (mode: InputFocusMode) => void) => () => void;
  actionBarRoot?: HTMLElement;
  touchControlsRoot?: HTMLElement;
  touchControlsPreferences?: () => TouchControlPreferences;
  subscribeTouchControlsPreferences?: (listener: (preferences: TouchControlPreferences) => void) => () => void;
  abilitySlots?: readonly AbilitySlotBinding[];
  /**
   * Optional visual map bundle. Resolves independently of the physics bundle;
   * the scene mounts the placeholder collider visuals first and swaps to the
   * loaded glTF group once this promise resolves.
   */
  visual?: Promise<LoadedVisualMap | undefined>;
};

const AIM_FALLBACK_DISTANCE = 36;
const GROUND_RAY_MAX_TOI = 600;
const MIN_GROUND_NORMAL_Y = 0.45;
const CAMERA_FOLLOW_LERP = 0.08;
const TOUCH_LOOK_STICK_PIXELS_PER_SECOND = 380;
const JUMP_ANIMATION_VERTICAL_SPEED = 4.0;

type ComputedAbilityAim = AbilityAimState & {
  aimPoint: Vec3;
  rayOrigin: Vec3;
  rayDirection: Vec3;
  hasGround: boolean;
  validGround: boolean;
  canvasX: number;
  canvasY: number;
};

type PointerAbilityInteraction = {
  slot: AbilitySlot;
  pointerId: number;
  policy: AbilityInteractionPolicy;
  startClientX: number;
  startClientY: number;
  aiming: boolean;
};

type ActiveCharge = {
  slot: AbilitySlot;
  ability: AbilityCatalogEntry;
  startedAtMs: number;
};

type LocalCooldown = {
  ability: AbilityCatalogEntry;
  startedTick: bigint;
  // Authoritative duration in ticks. Predicted from catalog at
  // press-time; overwritten by CastStart.effective_cooldown_ticks
  // once the server-issued event arrives. See
  // docs/contracts/ability_cast_lifecycle_contract.md.
  effectiveTicks: number;
};

export async function mountScene(
  root: HTMLElement,
  stdbClient: StdbClient,
  bundle: LoadedMapBundle = getStartupMapBundle(),
  options: SceneOptions = {},
): Promise<SceneHandle> {
  const physics = await buildStaticPhysicsWorld(bundle);

  const renderer = new THREE.WebGLRenderer({
    antialias: true,
    alpha: false,
    preserveDrawingBuffer: true,
  });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
  renderer.setSize(root.clientWidth, root.clientHeight);
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.domElement.dataset.testid = 'scene-canvas';
  root.append(renderer.domElement);

  const abilitySlots = options.abilitySlots ?? DEFAULT_ABILITY_SLOTS;
  let abilityHud: ActionBarHandle | undefined;
  let touchMovementControls: TouchMovementControlsHandle | undefined;
  let touchLookControls: TouchLookControlsHandle | undefined;

  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0x101417);
  const camera = new THREE.PerspectiveCamera(55, 1, 0.1, 1400);

  // Set up Bloom Post-Processing
  const renderScene = new RenderPass(scene, camera);
  const bloomPass = new UnrealBloomPass(new THREE.Vector2(root.clientWidth, root.clientHeight), 1.5, 0.4, 0.85);
  bloomPass.threshold = 1.0; // Only bloom items with emissive intensity > 1.0
  bloomPass.strength = 1.2; // Intensity
  bloomPass.radius = 0.5;

  const composer = new EffectComposer(renderer);
  composer.addPass(renderScene);
  composer.addPass(bloomPass);

  const vfxManager = new VfxManager();
  scene.add(vfxManager.group);
  const floatingOverlay = new FloatingOverlayLayer(root);

  const cleanups: Array<() => void> = [];
  let disposed = false;
  const sceneToken: object = { id: Symbol('dive-scene') };
  const runCleanup = () => {
    if (disposed) {
      return;
    }
    disposed = true;
    for (let idx = cleanups.length - 1; idx >= 0; idx -= 1) {
      try {
        cleanups[idx]!();
      } catch (err) {
        console.error('scene cleanup step failed', err);
      }
    }
  };

  cleanups.push(() => {
    abilityHud?.destroy();
    touchMovementControls?.destroy();
    touchLookControls?.destroy();
    vfxManager.dispose();
    floatingOverlay.destroy();
    renderer.dispose();
    renderer.domElement.remove();
  });
  cleanups.push(() => physics.destroy());
  cleanups.push(() => disposeThreeObject(scene));

  try {
    const cameraRig = new OrbitCameraRig();
    const initialCameraFrame = cameraRig.frame({ x: 0, y: 0.9, z: 0 });
    camera.position.set(initialCameraFrame.position.x, initialCameraFrame.position.y, initialCameraFrame.position.z);
    camera.lookAt(initialCameraFrame.target.x, initialCameraFrame.target.y, initialCameraFrame.target.z);

  const playerBody = physics.world.createRigidBody(
    RAPIER.RigidBodyDesc.kinematicPositionBased().setTranslation(0, 1.05, 0),
  );
  const playerCollider = physics.world.createCollider(
    RAPIER.ColliderDesc.capsule(PLAYER_CAPSULE_HALF_HEIGHT, PLAYER_CAPSULE_RADIUS),
    playerBody,
  );
  physics.step();
  const playerController = new PlayerCharacterController(physics.world);
  cleanups.push(() => playerController.destroy());
  const predictionPhysics = new ClientPhysicsPrediction(physics.world, playerBody, playerCollider);
  cleanups.push(() => predictionPhysics.destroy());
  const staticMovementFilter = predictionPhysics.movementFilterPredicate();

  const hemi = new THREE.HemisphereLight(0xc9e5ff, 0x2b2018, 1.8);
  scene.add(hemi);

  const sun = new THREE.DirectionalLight(0xfff1d0, 2.4);
  sun.position.set(7, 10, 4);
  scene.add(sun);

  const colliderGroup = new THREE.Group();
  colliderGroup.name = 'contract-map-colliders';
  for (const collider of bundle.colliders.colliders) {
    const mesh = meshForCollider(collider);
    if (mesh) {
      colliderGroup.add(mesh);
    }
  }
  scene.add(colliderGroup);

  // Visual channel lands asynchronously. Until it does (or if there is no
  // visual bundle at all), the collider placeholders above are the only
  // ground/structure the player sees. When the glTF arrives we hide the
  // placeholders and attach the artist-authored mesh instead.
  let visualGroup: THREE.Group | undefined;
  let visualDispose: (() => void) | undefined;
  if (options.visual) {
    void options.visual
      .then((visual) => {
        if (disposed || !visual) {
          visual?.dispose();
          return;
        }
        visualGroup = visual.group;
        visualDispose = visual.dispose;
        colliderGroup.visible = false;
        scene.add(visualGroup);
      })
      .catch((error: unknown) => {
        console.warn(`visual map bundle failed to load; staying on collider placeholders`, error);
      });
  }
  cleanups.push(() => {
    if (visualGroup) {
      scene.remove(visualGroup);
    }
    visualDispose?.();
  });

  const gridSize = bundle.manifest.source.kind === 'layer' ? 48 : 44;
  const grid = new THREE.GridHelper(gridSize, gridSize, 0x71d7c4, 0x34464b);
  grid.position.y = 0.015;
  scene.add(grid);

  const characterLoader = new CharacterModelLoader();

  const playerGroup = new THREE.Group();
  playerGroup.position.set(0, 0, 0);
  scene.add(playerGroup);

  const playerFallback = new THREE.Mesh(
    new THREE.CapsuleGeometry(0.35, 0.9, 6, 12),
    new THREE.MeshStandardMaterial({ color: 0xf4c95d, metalness: 0.08, roughness: 0.45 }),
  );
  playerFallback.position.y = 0.9;
  playerGroup.add(playerFallback);

  let localCharacterModel: CharacterModel | undefined;
  void characterLoader.load().then((gltfGroup) => {
    if (disposed) return;
    localCharacterModel = new CharacterModel(gltfGroup);
    playerGroup.remove(playerFallback);
    disposeThreeObject(playerFallback);
    playerGroup.add(localCharacterModel.group);
  }).catch((err) => console.warn('Failed to load local character model', err));

  // Caster attachment for VFX that should track the local player's
  // position/facing (capsule sweeps, PBAoE auras). Uses the live kinematic
  // body + player group rotation so prediction motion is reflected.
  const localCasterAttachment: CasterAttachment = {
    position: () => {
      const t = playerBody.translation();
      return new THREE.Vector3(t.x, t.y - 1.05, t.z);
    },
    rotation: () => new THREE.Quaternion().setFromEuler(new THREE.Euler(0, playerGroup.rotation.y, 0)),
  };

  function remoteCasterAttachment(entityId: bigint): CasterAttachment {
    return {
      position: () => {
        const p = remoteWorld.getPosition(entityId);
        return p ? new THREE.Vector3(p.x, p.y, p.z) : undefined;
      },
      rotation: () => {
        const record = remoteWorld.getRecord(entityId);
        return record ? record.mesh.quaternion.clone() : undefined;
      },
    };
  }

  const marker = new THREE.Mesh(
    new THREE.TorusGeometry(1.15, 0.025, 8, 64),
    new THREE.MeshStandardMaterial({ color: 0x7fd1ff, emissive: 0x124761, emissiveIntensity: 0.65 }),
  );
  marker.rotation.x = Math.PI / 2;
  marker.position.y = 0.04;
  scene.add(marker);

  const aimLineGeometry = new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(), new THREE.Vector3()]);
  const aimLineMaterial = new THREE.LineBasicMaterial({ color: 0x9bdcff, transparent: true, opacity: 0.78 });
  const aimLine = new THREE.Line(aimLineGeometry, aimLineMaterial);
  aimLine.name = 'ability-aim-line';
  scene.add(aimLine);

  const groundReticleMaterial = new THREE.MeshStandardMaterial({
    color: 0x65e08d,
    emissive: 0x12381f,
    emissiveIntensity: 0.75,
    transparent: true,
    opacity: 0.88,
  });
  const groundReticle = new THREE.Mesh(new THREE.TorusGeometry(1, 0.035, 8, 72), groundReticleMaterial);
  groundReticle.name = 'ability-ground-reticle';
  groundReticle.rotation.x = Math.PI / 2;
  groundReticle.visible = false;
  scene.add(groundReticle);

  let frame = 0;
  let lastSnapshot: StdbSnapshot | undefined;
  let lastServerTransformTick: bigint | undefined;
  let lastUpdateMs = performance.now();
  const moveInput = { x: 0, z: 0 };
  const movementSources = {
    keyboard: { x: 0, z: 0 },
    touch: { x: 0, z: 0 },
  };
  const lookInput = { x: 0, y: 0 };
  let lastAction: IntentAction = stopIntent();
  let focusedAbilitySlot: AbilitySlot = 1;
  let armedAbilitySlot: AbilitySlot = 1;
  let lastSubmittedAbilityId: number | undefined;
  let lastReleasedAbilityId: number | undefined;
  let lastSubmitSuppressed: string | undefined;
  let pointerCanvasPosition: { x: number; y: number } | undefined;
  let actionAimOverride: { x: number; y: number } | undefined;
  let cameraCaptureDesired = false;
  let cameraCaptureActive = false;
  let cameraCapturePendingClick = false;
  let lastActionInputGesture: string | undefined;
  let lastInvalidGround = false;
  let lastAimRaycastMs = 0;
  let lastKccMoveMs = 0;
  let lastRemoteIngestMs = 0;
  const aimRaycaster = new THREE.Raycaster();
  const remoteWorld = new RemoteEntityWorld(scene, characterLoader);
  cleanups.push(() => remoteWorld.destroy());
  const visualDirector = new VisualDirector({
    vfxManager,
    floatingOverlay,
    localCaster: localCasterAttachment,
    localPosition: () => {
      const t = playerBody.translation();
      return new THREE.Vector3(t.x, t.y - 1.05, t.z);
    },
    localCharacterModel: () => localCharacterModel,
    remotePosition: (entityId) => {
      const p = remoteWorld.getPosition(entityId);
      return p ? new THREE.Vector3(p.x, p.y, p.z) : undefined;
    },
    remoteCaster: remoteCasterAttachment,
    playRemoteAnimation: (entityId, request) => remoteWorld.triggerSkillAnimation(entityId, request),
    cancelRemoteAnimation: (entityId) => remoteWorld.cancelSkillAnimation(entityId),
  });
  let lastLocalCharacterY = playerBody.translation().y;
  let localJumpRising = false;
  const targetSelection = new TargetSelection();
  const coarsePointerQuery =
    typeof window !== 'undefined' && typeof window.matchMedia === 'function'
      ? window.matchMedia('(pointer: coarse)')
      : undefined;
  const isCoarsePointer = (): boolean => coarsePointerQuery?.matches ?? false;
  let tabCandidateCount = 0;
  const localPlayer = new LocalPlayerReconciler({ replayMovement: predictionPhysics.movementResolver() });
  const pointerAbilityInteractions = new Map<number, PointerAbilityInteraction>();
  const activeCharges = new Map<AbilitySlot, ActiveCharge>();
  const localCooldowns = new Map<number, LocalCooldown>();
  // Cursor for combat-event reconciliation. We re-process every event
  // whose eventId exceeds this cursor on each snapshot tick.
  let lastReconciledCombatEventId = 0n;

  function reconcileLocalCooldownsFromSnapshot(snapshot: StdbSnapshot): void {
    const ownId = snapshot.entityId;
    if (ownId === undefined) {
      return;
    }
    const events = snapshot.combatEvents;
    if (!events || events.length === 0) {
      return;
    }
    let cursor = lastReconciledCombatEventId;
    const pending = events.filter((row) => row.eventId > cursor);
    if (pending.length === 0) {
      return;
    }
    pending.sort((a, b) => {
      if (a.tickId !== b.tickId) return a.tickId < b.tickId ? -1 : 1;
      if (a.eventSequence !== b.eventSequence) return a.eventSequence - b.eventSequence;
      return a.eventId < b.eventId ? -1 : a.eventId > b.eventId ? 1 : 0;
    });
    for (const row of pending) {
      if (row.eventId > cursor) cursor = row.eventId;
      const kind = row.eventKind;
      visualDirector.handleCombatEvent(row, ownId);
      switch (kind.tag) {
        case 'CastStart': {
          const abilityId = kind.value.abilityId;
          const effective = kind.value.effectiveCooldownTicks;
          if (row.sourceEntity !== ownId) break;
          const binding = abilitySlots.find((b) => b.ability.abilityId === abilityId);
          const slotAbility = binding?.ability;
          if (slotAbility === undefined || effective <= 0) break;
          localCooldowns.set(abilityId, {
            ability: slotAbility,
            startedTick: row.tickId,
            effectiveTicks: effective,
          });
          break;
        }
        case 'AbilityCancelled': {
          const abilityId = kind.value.abilityId;
          if (row.sourceEntity !== ownId) break;
          // Any server cancel ends speculative local cast/charge UI. Only
          // Death currently authorizes a cooldown clear/refund; HardCc keeps
          // the cooldown established by CastStart.
          for (const [slot, charge] of Array.from(activeCharges.entries())) {
            if (charge.ability.abilityId === abilityId) {
              activeCharges.delete(slot);
            }
          }
          if (kind.value.reason.tag === 'Death') {
            localCooldowns.delete(abilityId);
          }
          break;
        }
        default:
          break;
      }
    }
    lastReconciledCombatEventId = cursor;
  }

  const resize = () => {
    const width = root.clientWidth || window.innerWidth;
    const height = root.clientHeight || window.innerHeight;
    camera.aspect = width / height;
    camera.updateProjectionMatrix();
    renderer.setSize(width, height);
    composer.setSize(width, height);
  };

  const inputKeys = new Set(['KeyW', 'KeyA', 'KeyS', 'KeyD', 'ArrowUp', 'ArrowLeft', 'ArrowDown', 'ArrowRight']);

  const keyStates = new Set<string>();

  // 20 Hz Move re-submission while a direction is held; Stop sent once on release.
  const INTENT_HZ = 20;
  const INTENT_PERIOD_MS = 1000 / INTENT_HZ;
  let lastMoveIntentAtMs = -Infinity;
  let blockHeld = false;
  let lastBlockIntentAtMs = -Infinity;

  const updateMovementKeys = () => {
    const rawX = (keyStates.has('KeyD') || keyStates.has('ArrowRight') ? 1 : 0) -
      (keyStates.has('KeyA') || keyStates.has('ArrowLeft') ? 1 : 0);
    const rawZ = (keyStates.has('KeyS') || keyStates.has('ArrowDown') ? 1 : 0) -
      (keyStates.has('KeyW') || keyStates.has('ArrowUp') ? 1 : 0);
    setMovementSource('keyboard', rawX, rawZ);
  };

  const setTouchMovement = (movement: TouchMovementVector) => {
    setMovementSource('touch', movement.x, movement.z);
  };

  const stopTouchMovement = () => {
    setMovementSource('touch', 0, 0);
  };

  const cancelTouchMovement = () => {
    touchMovementControls?.cancel();
    stopTouchMovement();
  };

  const setTouchLook = (look: TouchLookVector) => {
    lookInput.x = look.x;
    lookInput.y = look.y;
    pointerCanvasPosition = undefined;
  };

  const stopTouchLook = () => {
    lookInput.x = 0;
    lookInput.y = 0;
  };

  const cancelTouchLook = () => {
    touchLookControls?.cancel();
    stopTouchLook();
  };

  function clearBlockInput(): void {
    blockHeld = false;
    lastBlockIntentAtMs = -Infinity;
  }

  function toggleCameraCapture(): void {
    if (cameraCaptureActive || cameraCaptureDesired) {
      releaseCameraCapture();
      return;
    }
    requestCameraCapture();
  }

  function requestCameraCapture(): void {
    if ((options.inputMode?.() ?? 'gameplay') !== 'gameplay') {
      cameraCaptureDesired = false;
      cameraCapturePendingClick = false;
      return;
    }

    cameraCaptureDesired = true;
    cameraCapturePendingClick = true;
    pointerCanvasPosition = undefined;

    const request = renderer.domElement.requestPointerLock;
    if (typeof request !== 'function') {
      return;
    }

    try {
      const result = request.call(renderer.domElement) as Promise<void> | void;
      if (result && typeof result.catch === 'function') {
        result.catch(() => {
          if (cameraCaptureDesired) {
            cameraCapturePendingClick = true;
          }
        });
      }
    } catch {
      cameraCapturePendingClick = true;
    }
  }

  function releaseCameraCapture(): void {
    cameraCaptureDesired = false;
    cameraCapturePendingClick = false;
    pointerCanvasPosition = undefined;
    if (document.pointerLockElement === renderer.domElement && typeof document.exitPointerLock === 'function') {
      document.exitPointerLock();
    }
    cameraCaptureActive = document.pointerLockElement === renderer.domElement;
  }

  function handlePointerLockChange(): void {
    const active = document.pointerLockElement === renderer.domElement;
    const wasActive = cameraCaptureActive;
    cameraCaptureActive = active;
    if (active) {
      cameraCapturePendingClick = false;
      pointerCanvasPosition = undefined;
      return;
    }
    if (wasActive) {
      cameraCaptureDesired = false;
      cameraCapturePendingClick = false;
    }
  }

  function handlePointerLockError(): void {
    if (cameraCaptureDesired) {
      cameraCapturePendingClick = true;
    }
  }

  function currentCameraFollowTarget(): { x: number; y: number; z: number } {
    const translation = playerBody.translation();
    return {
      x: translation.x,
      y: translation.y + 0.9,
      z: translation.z,
    };
  }

  function setMovementSource(source: keyof typeof movementSources, x: number, z: number): void {
    movementSources[source].x = x;
    movementSources[source].z = z;
    syncMovementInput();
  }

  function syncMovementInput(): void {
    const canSendInput = inputAllowed(lastSnapshot, options);
    const canSendStop = reducerInputAllowed(lastSnapshot, options);
    const raw = activeMovementSource();
    const next = canSendInput ? cameraRelativeMovement(raw.x, raw.z) : { x: 0, z: 0 };
    const nextX = next.x;
    const nextZ = next.z;
    const hadInput = moveInput.x !== 0 || moveInput.z !== 0;
    moveInput.x = nextX;
    moveInput.z = nextZ;
    const hasInput = raw.x !== 0 || raw.z !== 0;

    if ((!hasInput || !canSendInput) && hadInput) {
      // Released: send Stop immediately, reset throttle.
      lastAction = stopIntent();
      lastMoveIntentAtMs = -Infinity;
      if (canSendStop) {
        void stdbClient.submitIntent(lastAction);
      }
    } else if (hasInput && !hadInput && canSendInput) {
      // Just pressed: send first Move immediately for low latency.
      const normalized = normalizeDirection(nextX, nextZ);
      lastAction = moveIntent(normalized.x, 0, normalized.z);
      lastMoveIntentAtMs = performance.now();
      void stdbClient.submitIntent(lastAction);
    }
    // Direction change while held is handled by the 20 Hz throttle in animate().
  }

  function refreshMoveInputForFrame(canSendInput: boolean): void {
    if (!canSendInput) {
      moveInput.x = 0;
      moveInput.z = 0;
      return;
    }
    const raw = activeMovementSource();
    const next = cameraRelativeMovement(raw.x, raw.z);
    moveInput.x = next.x;
    moveInput.z = next.z;
  }

  function cameraRelativeMovement(x: number, z: number): { x: number; z: number } {
    return rotateMovementByCameraYaw(x, z, cameraRig.snapshot().yaw);
  }

  function applyTouchLook(deltaSeconds: number): void {
    if (lookInput.x === 0 && lookInput.y === 0) {
      return;
    }
    cameraRig.rotate(
      lookInput.x * TOUCH_LOOK_STICK_PIXELS_PER_SECOND * deltaSeconds,
      lookInput.y * TOUCH_LOOK_STICK_PIXELS_PER_SECOND * deltaSeconds,
    );
    pointerCanvasPosition = undefined;
  }

  function activeMovementSource(): { x: number; z: number } {
    if (movementSources.touch.x !== 0 || movementSources.touch.z !== 0) {
      return movementSources.touch;
    }
    return movementSources.keyboard;
  }

  const clearGameplayKeys = () => {
    if (keyStates.size === 0) {
      return;
    }
    keyStates.clear();
    updateMovementKeys();
  };

  const gameplayKeyboardAllowed = (event: KeyboardEvent) =>
    gameplayKeyboardInputAllowed(event, options.inputMode?.() ?? 'gameplay');

  const onKeyDown = (event: KeyboardEvent) => {
    if (!gameplayKeyboardAllowed(event)) {
      releaseCameraCapture();
      clearBlockInput();
      clearGameplayKeys();
      return;
    }

    if ((event.code === 'AltLeft' || event.code === 'AltRight') && !event.repeat) {
      event.preventDefault();
      toggleCameraCapture();
      return;
    }

    if (event.code === 'KeyC' && !event.repeat) {
      event.preventDefault();
      cameraRig.toggleDetached(currentCameraFollowTarget());
      pointerCanvasPosition = undefined;
      syncMovementInput();
      return;
    }

    if (event.code === 'Tab' && !event.repeat) {
      event.preventDefault();
      cycleTabTarget();
      return;
    }

    if (event.code === 'KeyE' && !event.repeat) {
      event.preventDefault();
      triggerInteract();
      return;
    }

    if (event.code === 'Space' && !event.repeat) {
      event.preventDefault();
      submitJumpIntent();
      return;
    }

    if (event.code === 'KeyX' && !event.repeat) {
      event.preventDefault();
      submitWeaponSwapIntent();
      return;
    }

    if (event.code === 'ShiftLeft' || event.code === 'ShiftRight') {
      event.preventDefault();
      if (!event.repeat && !blockHeld) {
        blockHeld = true;
        submitBlockIntent();
      }
      return;
    }

    const abilitySlot = abilitySlotFromKeyboardEvent(event);
    if (abilitySlot !== undefined) {
      event.preventDefault();
      if (!event.repeat) {
        pressAbilitySlot(abilitySlot);
      }
      return;
    }

    if (!inputKeys.has(event.code)) {
      return;
    }
    event.preventDefault();
    if (!keyStates.has(event.code)) {
      keyStates.add(event.code);
      updateMovementKeys();
    }
  };

  const onKeyUp = (event: KeyboardEvent) => {
    if (!gameplayKeyboardAllowed(event)) {
      clearBlockInput();
      clearGameplayKeys();
      return;
    }

    if (event.code === 'AltLeft' || event.code === 'AltRight') {
      event.preventDefault();
      return;
    }

    if (event.code === 'ShiftLeft' || event.code === 'ShiftRight') {
      event.preventDefault();
      clearBlockInput();
      return;
    }

    const abilitySlot = abilitySlotFromKeyboardEvent(event);
    if (abilitySlot !== undefined) {
      event.preventDefault();
      releaseAbilitySlot(abilitySlot);
      return;
    }

    if (!inputKeys.has(event.code)) {
      return;
    }
    event.preventDefault();
    if (keyStates.delete(event.code)) {
      updateMovementKeys();
    }
  };

  const onFocusIn = (event: FocusEvent) => {
    if ((options.inputMode?.() ?? 'gameplay') !== 'gameplay' || isUiKeyboardTarget(event.target)) {
      releaseCameraCapture();
      clearBlockInput();
      clearGameplayKeys();
      cancelTouchMovement();
      cancelTouchLook();
      releaseAllActiveCharges();
      cancelPointerAbilityInteractions();
    }
  };

  const onWindowBlur = () => {
    releaseCameraCapture();
    clearBlockInput();
    clearGameplayKeys();
    cancelTouchMovement();
    cancelTouchLook();
    releaseAllActiveCharges();
    cancelPointerAbilityInteractions();
  };

  function focusAbilitySlot(slot: AbilitySlot): void {
    focusedAbilitySlot = slot;
  }

  function armAbilitySlot(slot: AbilitySlot): void {
    focusedAbilitySlot = slot;
    armedAbilitySlot = slot;
  }

  function currentAimAbilitySlot(): AbilitySlot {
    return armedAbilitySlot;
  }

  function onActionPointerDown(slot: AbilitySlot, event: PointerEvent): void {
    const binding = bindingForSlot(slot);
    if (!binding) {
      return;
    }
    focusAbilitySlot(slot);
    const policy = deriveAbilityInteractionPolicy(binding.ability);
    if (policy.kind === 'disabled') {
      lastSubmitSuppressed = 'disabled';
      refreshAimFeedback();
      updateActionBar();
      return;
    }
    armAbilitySlot(slot);
    if (policy.kind === 'instant') {
      pressAbilitySlot(slot);
      return;
    }

    const interaction: PointerAbilityInteraction = {
      slot,
      pointerId: event.pointerId,
      policy,
      startClientX: event.clientX,
      startClientY: event.clientY,
      aiming: false,
    };

    if (policy.kind === 'charge_release' && !beginChargeSlot(slot)) {
      return;
    }

    pointerAbilityInteractions.set(event.pointerId, interaction);
    refreshAimFeedback();
    updateActionBar();
  }

  function onActionPointerMove(slot: AbilitySlot, event: PointerEvent): void {
    const interaction = pointerAbilityInteractions.get(event.pointerId);
    if (!interaction || interaction.slot !== slot || interaction.policy.kind !== 'aim_release') {
      return;
    }
    const dx = event.clientX - interaction.startClientX;
    const dy = event.clientY - interaction.startClientY;
    if (!interaction.aiming && Math.hypot(dx, dy) < 4) {
      return;
    }
    interaction.aiming = true;
    actionAimOverride = actionAimOverrideFromDelta(dx, dy);
    refreshAimFeedback();
    updateActionBar();
  }

  function onActionPointerUp(slot: AbilitySlot, event: PointerEvent): void {
    const interaction = pointerAbilityInteractions.get(event.pointerId);
    if (!interaction || interaction.slot !== slot) {
      return;
    }
    pointerAbilityInteractions.delete(event.pointerId);
    if (interaction.policy.kind === 'charge_release') {
      releaseAbilitySlot(slot);
    } else if (interaction.policy.kind === 'aim_release') {
      submitAbilitySlot(slot);
    }
    actionAimOverride = undefined;
    updateActionBar();
  }

  function onActionPointerCancel(slot: AbilitySlot, event: PointerEvent): void {
    const interaction = pointerAbilityInteractions.get(event.pointerId);
    if (!interaction || interaction.slot !== slot) {
      return;
    }
    pointerAbilityInteractions.delete(event.pointerId);
    if (interaction.policy.kind === 'charge_release') {
      releaseAbilitySlot(slot);
    }
    actionAimOverride = undefined;
    refreshAimFeedback();
    updateActionBar();
  }

  function onActionLongPress(slot: AbilitySlot): void {
    focusAbilitySlot(slot);
    lastActionInputGesture = 'long_press';
    refreshAimFeedback();
    updateActionBar();
  }

  const onPointerMove = (event: PointerEvent) => {
    if (isTouchLikePointer(event) && (options.inputMode?.() ?? 'gameplay') === 'gameplay') {
      event.preventDefault();
    }

    if (cameraCaptureActive) {
      pointerCanvasPosition = undefined;
      return;
    }

    const rect = renderer.domElement.getBoundingClientRect();
    pointerCanvasPosition = {
      x: Math.max(0, Math.min(rect.width, event.clientX - rect.left)),
      y: Math.max(0, Math.min(rect.height, event.clientY - rect.top)),
    };
  };

  const onPointerLeave = () => {
    if (!cameraCaptureActive) {
      pointerCanvasPosition = undefined;
    }
  };

  const onCanvasPointerDown = (event: PointerEvent) => {
    if (cameraCaptureDesired && cameraCapturePendingClick && event.pointerType !== 'touch') {
      event.preventDefault();
      requestCameraCapture();
      return;
    }

    if (cameraCaptureActive) {
      return;
    }

    if (isTouchLikePointer(event)) {
      event.preventDefault();
    }

    if (event.button !== 0) {
      // Right-click cancels selection; middle is ignored.
      if (event.button === 2) {
        targetSelection.setSelected(undefined);
      }
      return;
    }
    if ((options.inputMode?.() ?? 'gameplay') !== 'gameplay') {
      return;
    }
    // Update aim from this pointer position before reading hover/aim.
    const rect = renderer.domElement.getBoundingClientRect();
    pointerCanvasPosition = {
      x: Math.max(0, Math.min(rect.width, event.clientX - rect.left)),
      y: Math.max(0, Math.min(rect.height, event.clientY - rect.top)),
    };
    const activeAbilitySlot = currentAimAbilitySlot();
    const ability = abilityForSceneSlot(activeAbilitySlot);
    if (ability && ability.targetingMode === 'ground_target') {
      // Click-to-place ground AOE: reuse the existing aim path.
      submitAbilitySlot(activeAbilitySlot);
      return;
    }
    // Otherwise: promote hover → selected, or clear if the click landed on empty space.
    const playerPosition = vec3FromVector(playerBody.translation());
    const aim = computeCurrentAbilityAim(playerPosition);
    if (aim.softTarget !== undefined && aim.softTarget !== lastSnapshot?.entityId) {
      targetSelection.setSelected(aim.softTarget);
    } else {
      targetSelection.setSelected(undefined);
    }
  };

  const onCanvasPointerUp = (event: PointerEvent) => {
    if (isTouchLikePointer(event)) {
      event.preventDefault();
    }
  };

  const onCanvasPointerCancel = (event: PointerEvent) => {
    if (isTouchLikePointer(event)) {
      event.preventDefault();
    }
  };

  const onDocumentMouseMove = (event: MouseEvent) => {
    if (!cameraCaptureActive || (options.inputMode?.() ?? 'gameplay') !== 'gameplay') {
      return;
    }
    event.preventDefault();
    cameraRig.rotate(event.movementX, event.movementY);
    pointerCanvasPosition = undefined;
    syncMovementInput();
  };

  const onCanvasWheel = (event: WheelEvent) => {
    if ((options.inputMode?.() ?? 'gameplay') !== 'gameplay') {
      return;
    }
    event.preventDefault();
    cameraRig.zoom(event.deltaY);
    syncMovementInput();
  };

  function isTouchLikePointer(event: PointerEvent): boolean {
    return event.pointerType === 'touch' || event.pointerType === 'pen' || event.pointerType === '';
  }

  function triggerInteract(): void {
    if (!inputAllowed(lastSnapshot, options) || !lastSnapshot) {
      return;
    }

    const target = resolveInteractTarget({
      selectedEntity: targetSelection.getSelected(),
      hoverEntity: targetSelection.getHover(),
      loot: buildLootHudModel(lastSnapshot),
    });
    if (!target) {
      return;
    }

    if (target.kind === 'entity') {
      void stdbClient.submitIntent(interactIntent(target.entityId));
      return;
    }

    void stdbClient.claimLoot(target.lootPileId, target.itemId, target.targetSlot).catch(() => undefined);
  }

  function submitJumpIntent(): void {
    if (!inputAllowed(lastSnapshot, options)) {
      return;
    }
    void stdbClient.submitIntent(jumpIntent());
  }

  function submitWeaponSwapIntent(): void {
    if (!inputAllowed(lastSnapshot, options)) {
      return;
    }
    void stdbClient.submitIntent(weaponSwapIntent());
  }

  function submitBlockIntent(nowMs: number = performance.now()): void {
    if (!inputAllowed(lastSnapshot, options)) {
      return;
    }
    const playerPosition = vec3FromVector(playerBody.translation());
    const aim = computeCurrentAbilityAim(playerPosition);
    const lookDir = horizontalLookDirection(aim.aimDirection) ?? cameraForwardOnGround();
    void stdbClient.submitIntent(blockIntent(lookDir.x, 0, lookDir.z));
    lastBlockIntentAtMs = nowMs;
  }

  function horizontalLookDirection(direction: Vec3 | undefined): { x: number; z: number } | undefined {
    if (!direction) {
      return undefined;
    }
    const normalized = normalizeDirection(direction.x, direction.z);
    if (normalized.x === 0 && normalized.z === 0) {
      return undefined;
    }
    return normalized;
  }

  function cameraForwardOnGround(): { x: number; z: number } {
    return rotateMovementByCameraYaw(0, -1, cameraRig.snapshot().yaw);
  }

  function cycleTabTarget(): void {
    if (!lastSnapshot) {
      return;
    }
    const playerPosition = vec3FromVector(playerBody.translation());
    const ownId = lastSnapshot.entityId;
    const candidates = lastSnapshot.remoteTransforms
      .filter((t) => t.entityId !== ownId)
      .map((t) => ({
        entityId: t.entityId,
        position: { x: t.posX, y: t.posY + 0.35, z: t.posZ },
      }));
    tabCandidateCount = candidates.length;
    if (candidates.length === 0) {
      return;
    }
    const projector: ScreenProjector = (pos) => {
      const v = new THREE.Vector3(pos.x, pos.y, pos.z).project(camera);
      return {
        onScreen: v.x >= -1 && v.x <= 1 && v.y >= -1 && v.y <= 1 && v.z >= -1 && v.z <= 1,
        ndcX: v.x,
        ndcY: v.y,
      };
    };
    targetSelection.tabNext(candidates, playerPosition, projector);
    refreshAimFeedback();
  }

  function pressAbilitySlot(slot: AbilitySlot): void {
    const binding = bindingForSlot(slot);
    if (!binding) {
      return;
    }
    const policy = deriveAbilityInteractionPolicy(binding.ability);
    if (policy.kind === 'disabled') {
      focusAbilitySlot(slot);
      lastSubmitSuppressed = 'disabled';
      refreshAimFeedback();
      updateActionBar();
      return;
    }
    if (policy.kind === 'charge_release') {
      beginChargeSlot(slot);
      return;
    }
    submitAbilitySlot(slot);
  }

  function beginChargeSlot(slot: AbilitySlot): boolean {
    const ability = abilityForSceneSlot(slot);
    if (!ability || activeCharges.has(slot)) {
      return false;
    }
    armAbilitySlot(slot);
    const playerPosition = vec3FromVector(playerBody.translation());
    const aim = computeCurrentAbilityAim(playerPosition);

    if (!inputAllowed(lastSnapshot, options)) {
      lastSubmitSuppressed = 'input_disabled';
      updateAimFeedback(aim);
      return false;
    }

    const result = buildUseAbilityIntent(ability, aim);
    if (!result.ok) {
      lastSubmitSuppressed = result.reason;
      updateAimFeedback(aim);
      return false;
    }

    activeCharges.set(slot, {
      slot,
      ability,
      startedAtMs: performance.now(),
    });
    lastSubmitSuppressed = undefined;
    lastSubmittedAbilityId = ability.abilityId;
    updateAimFeedback(aim);
    updateActionBar();
    void stdbClient.submitIntent(result.action);
    return true;
  }

  function releaseAbilitySlot(slot: AbilitySlot): boolean {
    const activeCharge = activeCharges.get(slot);
    if (!activeCharge) {
      return false;
    }
    activeCharges.delete(slot);
    lastReleasedAbilityId = activeCharge.ability.abilityId;
    const predicted = localCooldownFraction(activeCharge.ability) <= 0;
    recordLocalCooldown(activeCharge.ability);
    void stdbClient.submitIntent(releaseAbilityAction(activeCharge.ability.abilityId));
    if (predicted) {
      // Charge releases don't carry a fresh aim result; reuse the local
      // caster origin for VFX. Ground-target charges fall back to the
      // forward-sweep approximation inside dispatchVfx.
      triggerLocalSkillPrediction(activeCharge.ability, undefined);
    }
    refreshAimFeedback();
    updateActionBar();
    return true;
  }

  function releaseAllActiveCharges(): void {
    for (const slot of [...activeCharges.keys()]) {
      releaseAbilitySlot(slot);
    }
  }

  function cancelPointerAbilityInteractions(): void {
    if (pointerAbilityInteractions.size === 0 && actionAimOverride === undefined) {
      return;
    }
    pointerAbilityInteractions.clear();
    actionAimOverride = undefined;
    refreshAimFeedback();
    updateActionBar();
  }

  const unsubscribeInputMode = options.subscribeInputMode?.((mode) => {
    if (mode !== 'gameplay') {
      clearGameplayKeys();
      cancelTouchMovement();
      cancelTouchLook();
      releaseAllActiveCharges();
      cancelPointerAbilityInteractions();
    }
  });
  if (unsubscribeInputMode) {
    cleanups.push(unsubscribeInputMode);
  }

  function submitAbilitySlot(slot: AbilitySlot): void {
    armAbilitySlot(slot);
    const ability = abilityForSceneSlot(slot);
    if (!ability) {
      return;
    }
    const playerPosition = vec3FromVector(playerBody.translation());
    const aim = computeCurrentAbilityAim(playerPosition);

    if (!inputAllowed(lastSnapshot, options)) {
      lastSubmitSuppressed = 'input_disabled';
      updateAimFeedback(aim);
      return;
    }

    const result = buildUseAbilityIntent(ability, aim);
    if (!result.ok) {
      lastSubmitSuppressed = result.reason;
      updateAimFeedback(aim);
      return;
    }

    lastSubmitSuppressed = undefined;
    lastSubmittedAbilityId = ability.abilityId;
    // Local-only visual prediction: only fire if no cooldown is already
    // active. recordLocalCooldown below makes this self-debouncing for spam.
    const predicted = localCooldownFraction(ability) <= 0;
    recordLocalCooldown(ability);
    updateAimFeedback(aim);
    updateActionBar();
    void stdbClient.submitIntent(result.action);
    if (predicted) {
      triggerLocalSkillPrediction(ability, result);
    }
  }

  function computeCurrentAbilityAim(playerPosition: Vec3): ComputedAbilityAim {
    const rect = renderer.domElement.getBoundingClientRect();
    const width = Math.max(1, rect.width);
    const height = Math.max(1, rect.height);
    const canvasX = actionAimOverride?.x ?? pointerCanvasPosition?.x ?? width / 2;
    const canvasY = actionAimOverride?.y ?? pointerCanvasPosition?.y ?? height / 2;
    const ndc = new THREE.Vector2((canvasX / width) * 2 - 1, -(canvasY / height) * 2 + 1);
    aimRaycaster.setFromCamera(ndc, camera);

    const rayOrigin = vec3FromVector(aimRaycaster.ray.origin);
    const rayDirection = vec3FromVector(aimRaycaster.ray.direction);
    const groundHit = groundHitForRay(rayOrigin, rayDirection);
    const softTargetRadius = isCoarsePointer() ? 1.4 : undefined;
    const ownEntityId = lastSnapshot?.entityId;
    const softTarget = findSoftTarget(
      rayOrigin,
      rayDirection,
      softTargetCandidatesFromTransforms(lastSnapshot?.remoteTransforms ?? [])
        .filter((candidate) => candidate.entityId !== ownEntityId)
        .map((c) => (softTargetRadius ? { ...c, radius: softTargetRadius } : c)),
    );
    targetSelection.setHover(softTarget?.entityId);
    const fallbackPoint = {
      x: rayOrigin.x + rayDirection.x * AIM_FALLBACK_DISTANCE,
      y: rayOrigin.y + rayDirection.y * AIM_FALLBACK_DISTANCE,
      z: rayOrigin.z + rayDirection.z * AIM_FALLBACK_DISTANCE,
    };
    const aimPoint = groundHit?.point ?? softTarget?.position ?? fallbackPoint;

    return {
      playerPosition,
      aimDirection: directionFromAimPoint(playerPosition, aimPoint, rayDirection),
      groundPoint: groundHit?.point,
      groundValid: groundHit?.valid ?? false,
      softTarget: softTarget?.entityId,
      selectedTarget: targetSelection.getSelected(),
      aimPoint,
      rayOrigin,
      rayDirection,
      hasGround: groundHit !== undefined,
      validGround: groundHit?.valid ?? false,
      canvasX,
      canvasY,
    };
  }

  function groundHitForRay(origin: Vec3, direction: Vec3): { point: Vec3; normal: Vec3; valid: boolean } | undefined {
    const ray = new RAPIER.Ray(origin, direction);
    const startedAtMs = performance.now();
    const hit = physics.world.castRayAndGetNormal(
      ray,
      GROUND_RAY_MAX_TOI,
      true,
      RAPIER.QueryFilterFlags.ONLY_FIXED,
      undefined,
      playerCollider,
    );
    lastAimRaycastMs = performance.now() - startedAtMs;
    if (!hit) {
      return undefined;
    }
    const point = vec3FromVector(ray.pointAt(hit.timeOfImpact));
    const normal = vec3FromVector(hit.normal);
    return {
      point,
      normal,
      valid: normal.y >= MIN_GROUND_NORMAL_Y,
    };
  }

  function updateAimFeedback(aim: ComputedAbilityAim): void {
    const aimSlot = currentAimAbilitySlot();
    const ability = abilityForSceneSlot(aimSlot);
    if (!ability) {
      return;
    }
    const result = buildUseAbilityIntent(ability, aim);
    const invalidGround = ability.targetingMode === 'ground_target' && (!result.ok || !aim.validGround);
    lastInvalidGround = invalidGround;
    const targetPoint = result.ok && result.targetPoint ? result.targetPoint : aim.aimPoint;
    const lineStart = {
      x: aim.playerPosition.x,
      y: aim.playerPosition.y + 0.35,
      z: aim.playerPosition.z,
    };

    setLineEndpoints(aimLineGeometry, lineStart, targetPoint);
    aimLineMaterial.color.setHex(invalidGround ? 0xff6b6b : 0x9bdcff);

    groundReticle.visible = ability.targetingMode === 'ground_target' && aim.hasGround;
    if (groundReticle.visible) {
      const point = result.ok && result.targetPoint ? result.targetPoint : aim.groundPoint ?? aim.aimPoint;
      const radius = previewRadius(ability.previewShape);
      groundReticle.position.set(point.x, point.y + 0.035, point.z);
      groundReticle.scale.set(radius, radius, radius);
      groundReticleMaterial.color.setHex(invalidGround ? 0xff6b6b : 0x65e08d);
      groundReticleMaterial.emissive.setHex(invalidGround ? 0x5a1111 : 0x12381f);
    }

    updateActionBar();
    if (window.__DIVE_WEB_ACTIVE_SCENE__ === sceneToken) {
      window.__DIVE_WEB_AIM_DIAGNOSTICS__ = {
        slot: aimSlot,
        focusedSlot: focusedAbilitySlot,
        armedSlot: armedAbilitySlot,
        abilityId: ability.abilityId,
        abilityName: ability.name,
        targeting: ability.targetingMode,
        hasGround: aim.hasGround,
        validGround: aim.validGround,
        softTarget: aim.softTarget,
        aimPoint: aim.aimPoint,
        aimDirection: aim.aimDirection,
        canvasX: aim.canvasX,
        canvasY: aim.canvasY,
        lastSubmittedAbilityId,
        lastReleasedAbilityId,
        lastActionInputGesture,
        lastSubmitSuppressed,
      };
    }
  }

  function refreshAimFeedback(): void {
    const playerPosition = vec3FromVector(playerBody.translation());
    updateAimFeedback(computeCurrentAbilityAim(playerPosition));
  }

  function updateActionBar(): void {
    if (!abilityHud) {
      return;
    }
    const states = new Map<AbilitySlot, ActionBarSlotVisualState>();
    for (const binding of abilitySlots) {
      const cooldownFraction = localCooldownFraction(binding.ability);
      const charge = activeCharges.get(binding.slot);
      const pointerInteraction = pointerInteractionForSlot(binding.slot);
      states.set(binding.slot, {
        focused: binding.slot === focusedAbilitySlot,
        armed: binding.slot === armedAbilitySlot,
        selected: binding.slot === focusedAbilitySlot,
        invalid: binding.slot === currentAimAbilitySlot() && lastInvalidGround,
        pressed: pointerInteraction !== undefined || charge !== undefined,
        charging: charge !== undefined,
        disabled: !binding.ability.enabled || deriveAbilityInteractionPolicy(binding.ability).kind === 'disabled',
        cooldownFraction,
        chargeFraction: charge ? activeChargeFraction(charge) : 0,
        chargeLabel: charge ? activeChargeLabel(charge) : undefined,
        aimPadX: pointerInteraction?.aiming ? actionPadOffset().x : 0,
        aimPadY: pointerInteraction?.aiming ? actionPadOffset().y : 0,
      });
    }
    abilityHud.update(states);
  }

  function bindingForSlot(slot: AbilitySlot): AbilitySlotBinding | undefined {
    return abilitySlots.find((binding) => binding.slot === slot);
  }

  function abilityForSceneSlot(slot: AbilitySlot): AbilityCatalogEntry | undefined {
    return bindingForSlot(slot)?.ability;
  }

  /**
   * Predicted local visual + animation for an accepted cast. Cooldown
   * gating is the caller's responsibility (we don't re-check here so charge
   * releases can fire as soon as the input lifts).
   */
  function triggerLocalSkillPrediction(
    ability: AbilityCatalogEntry,
    _intent: AbilityIntentResult | undefined,
  ): void {
    const visual = visualFor(ability);
    if (visual.characterAnimation) {
      localCharacterModel?.playOverlayAnimation(visual.characterAnimation);
    }
  }

  function recordLocalCooldown(ability: AbilityCatalogEntry): void {
    if (ability.cooldownTicks <= 0 || lastSnapshot?.latestTick === undefined) {
      return;
    }
    // Don't restart an already-running local cooldown when the player
    // repeatedly presses the slot. The authoritative CastStart event will
    // overwrite this with the server's effective_cooldown_ticks anyway;
    // until then we keep the timer monotonic so the UI sweep doesn't reset.
    if (localCooldownFraction(ability) > 0) {
      return;
    }
    localCooldowns.set(ability.abilityId, {
      ability,
      startedTick: lastSnapshot.latestTick,
      effectiveTicks: ability.cooldownTicks,
    });
  }

  function localCooldownFraction(ability: AbilityCatalogEntry): number {
    const cooldown = localCooldowns.get(ability.abilityId);
    if (!cooldown || lastSnapshot?.latestTick === undefined || cooldown.effectiveTicks <= 0) {
      return 0;
    }
    const elapsed = Number(lastSnapshot.latestTick - cooldown.startedTick);
    const remaining = Math.max(0, cooldown.effectiveTicks - elapsed);
    if (remaining === 0) {
      localCooldowns.delete(ability.abilityId);
      return 0;
    }
    return remaining / cooldown.effectiveTicks;
  }

  function activeChargeFraction(charge: ActiveCharge): number {
    const maxTicks = maxChargeTicks(charge.ability);
    if (maxTicks <= 0) {
      return 1;
    }
    return Math.max(0, Math.min(1, activeChargeTicks(charge) / maxTicks));
  }

  function activeChargeLabel(charge: ActiveCharge): string {
    const elapsedTicks = activeChargeTicks(charge);
    const tier = charge.ability.chargeTiers.filter((entry) => elapsedTicks >= entry.minTicks).length;
    return `T${Math.max(1, tier)}`;
  }

  function activeChargeTicks(charge: ActiveCharge): number {
    return Math.max(0, Math.floor(((performance.now() - charge.startedAtMs) / 1000) * ABILITY_TICK_RATE));
  }

  function maxChargeTicks(ability: AbilityCatalogEntry): number {
    return ability.chargeTiers.reduce((max, tier) => Math.max(max, tier.minTicks), 0);
  }

  function pointerInteractionForSlot(slot: AbilitySlot): PointerAbilityInteraction | undefined {
    for (const interaction of pointerAbilityInteractions.values()) {
      if (interaction.slot === slot) {
        return interaction;
      }
    }
    return undefined;
  }

  function actionAimOverrideFromDelta(dx: number, dy: number): { x: number; y: number } {
    const rect = renderer.domElement.getBoundingClientRect();
    const maxOffset = Math.max(48, Math.min(rect.width, rect.height) * 0.28);
    const x = rect.width / 2 + Math.max(-maxOffset, Math.min(maxOffset, dx * 1.6));
    const y = rect.height / 2 + Math.max(-maxOffset, Math.min(maxOffset, dy * 1.6));
    return { x, y };
  }

  function actionPadOffset(): { x: number; y: number } {
    if (!actionAimOverride) {
      return { x: 0, y: 0 };
    }
    const rect = renderer.domElement.getBoundingClientRect();
    return {
      x: Math.max(-18, Math.min(18, ((actionAimOverride.x - rect.width / 2) / Math.max(1, rect.width)) * 72)),
      y: Math.max(-18, Math.min(18, ((actionAimOverride.y - rect.height / 2) / Math.max(1, rect.height)) * 72)),
    };
  }

  abilityHud = createActionBar(
    options.actionBarRoot ?? root,
    {
      pointerDown: onActionPointerDown,
      pointerMove: onActionPointerMove,
      pointerUp: onActionPointerUp,
      pointerCancel: onActionPointerCancel,
      longPress: onActionLongPress,
    },
    abilitySlots,
  );
  if (options.touchControlsRoot) {
    const touchControlOptions = {
      preferences: options.touchControlsPreferences ?? (() => DEFAULT_TOUCH_CONTROL_PREFERENCES),
      subscribePreferences: options.subscribeTouchControlsPreferences,
      inputMode: options.inputMode,
      subscribeInputMode: options.subscribeInputMode,
    };
    touchMovementControls = createTouchMovementControls(options.touchControlsRoot, {
      ...touchControlOptions,
      onMove: setTouchMovement,
      onStop: stopTouchMovement,
    });
    touchLookControls = createTouchLookControls(options.touchControlsRoot, {
      ...touchControlOptions,
      onLook: setTouchLook,
      onStop: stopTouchLook,
    });
  }
  updateActionBar();

  const unsubscribe = stdbClient.subscribe((snapshot) => {
    const wasAllowed = inputAllowed(lastSnapshot, options);
    lastSnapshot = snapshot;
    reconcileLocalCooldownsFromSnapshot(snapshot);
    const isAllowed = inputAllowed(snapshot, options);

    const remoteIngestStartedAtMs = performance.now();
    remoteWorld.ingest(snapshot.remoteTransforms, snapshot.remoteEntities, remoteIngestStartedAtMs, snapshot.remoteHealth);
    lastRemoteIngestMs = performance.now() - remoteIngestStartedAtMs;
    const liveIds = new Set<bigint>();
    for (const t of snapshot.remoteTransforms) {
      liveIds.add(t.entityId);
    }
    targetSelection.reconcile(liveIds);

    if (!isAllowed) {
      cancelTouchMovement();
      clearBlockInput();
      moveInput.x = 0;
      moveInput.z = 0;
      lastMoveIntentAtMs = -Infinity;
    } else if (!wasAllowed && isAllowed) {
      syncMovementInput();
    }
    updateActionBar();

    const transform = snapshot.ownTransform;
    if (!transform) {
      return;
    }

    const measureCorrection = transform.lastTick !== lastServerTransformTick;
    lastServerTransformTick = transform.lastTick;
    const decision = localPlayer.reconcile(
      transform,
      snapshot.pendingIntents,
      playerBody.translation(),
      measureCorrection,
    );
    if (decision.shouldSnap) {
      playerController.snapTo(playerBody, decision.target);
    }
  });
  cleanups.push(() => unsubscribe());

  window.addEventListener('resize', resize);
  window.addEventListener('keydown', onKeyDown);
  window.addEventListener('keyup', onKeyUp);
  window.addEventListener('blur', onWindowBlur);
  document.addEventListener('focusin', onFocusIn);
  renderer.domElement.addEventListener('pointermove', onPointerMove);
  renderer.domElement.addEventListener('pointerleave', onPointerLeave);
  renderer.domElement.addEventListener('pointerdown', onCanvasPointerDown);
  renderer.domElement.addEventListener('pointerup', onCanvasPointerUp);
  renderer.domElement.addEventListener('pointercancel', onCanvasPointerCancel);
  renderer.domElement.addEventListener('wheel', onCanvasWheel, { passive: false });
  document.addEventListener('mousemove', onDocumentMouseMove);
  document.addEventListener('pointerlockchange', handlePointerLockChange);
  document.addEventListener('pointerlockerror', handlePointerLockError);
  const unsubscribeInputModeForCamera = options.subscribeInputMode?.((mode) => {
    if (mode !== 'gameplay') {
      releaseCameraCapture();
      cancelTouchLook();
    }
  });
  const onContextMenu = (event: MouseEvent) => {
    if (event.target === renderer.domElement) {
      event.preventDefault();
    }
  };
  renderer.domElement.addEventListener('contextmenu', onContextMenu);
  cleanups.push(() => {
    window.removeEventListener('resize', resize);
    window.removeEventListener('keydown', onKeyDown);
    window.removeEventListener('keyup', onKeyUp);
    window.removeEventListener('blur', onWindowBlur);
    document.removeEventListener('focusin', onFocusIn);
    renderer.domElement.removeEventListener('pointermove', onPointerMove);
    renderer.domElement.removeEventListener('pointerleave', onPointerLeave);
    renderer.domElement.removeEventListener('pointerdown', onCanvasPointerDown);
    renderer.domElement.removeEventListener('pointerup', onCanvasPointerUp);
    renderer.domElement.removeEventListener('pointercancel', onCanvasPointerCancel);
    renderer.domElement.removeEventListener('wheel', onCanvasWheel);
    document.removeEventListener('mousemove', onDocumentMouseMove);
    document.removeEventListener('pointerlockchange', handlePointerLockChange);
    document.removeEventListener('pointerlockerror', handlePointerLockError);
    unsubscribeInputModeForCamera?.();
    releaseCameraCapture();
    renderer.domElement.removeEventListener('contextmenu', onContextMenu);
  });
  resize();

  const renderNow = () => composer.render();

  const animate = () => {
    if (disposed) {
      return;
    }
    const nowMs = performance.now();
    const deltaSeconds = Math.min((nowMs - lastUpdateMs) / 1000, 0.05);
    lastUpdateMs = nowMs;
    frame += 1;

    const allowedThisFrame = inputAllowed(lastSnapshot, options);
    if (allowedThisFrame) {
      applyTouchLook(deltaSeconds);
    }
    refreshMoveInputForFrame(allowedThisFrame);
    const normalized = allowedThisFrame ? normalizeDirection(moveInput.x, moveInput.z) : { x: 0, z: 0 };
    if (normalized.x !== 0 || normalized.z !== 0) {
      // Re-submit Move at INTENT_HZ with a fresh sequence so the server keeps
      // applying movement each tick. Without this, a single Move runs for one
      // tick and the server stops while local prediction keeps moving — which
      // produces the "snap back on release" symptom.
      if (nowMs - lastMoveIntentAtMs >= INTENT_PERIOD_MS) {
        lastMoveIntentAtMs = nowMs;
        lastAction = moveIntent(normalized.x, 0, normalized.z);
        void stdbClient.submitIntent(lastAction);
      }

      playerGroup.rotation.y = Math.atan2(normalized.x, normalized.z);
    }

    if (blockHeld && allowedThisFrame && nowMs - lastBlockIntentAtMs >= INTENT_PERIOD_MS) {
      submitBlockIntent(nowMs);
    }

    const predicted = localPlayer.predictFrame(playerBody.translation(), normalized, deltaSeconds);
    const kccStartedAtMs = performance.now();
    const kccStats = playerController.moveToward(playerBody, playerCollider, predicted, deltaSeconds, {
      filterPredicate: staticMovementFilter,
    });
    lastKccMoveMs = performance.now() - kccStartedAtMs;
    physics.step();
    remoteWorld.step(lastSnapshot?.latestTick, deltaSeconds);
    remoteWorld.setHighlight(targetSelection.getSelected(), targetSelection.getHover());
    const translation = playerBody.translation();
    playerGroup.position.set(translation.x, translation.y - 1.05, translation.z);

    const velocity = playerBody.linvel();
    const speed = Math.hypot(velocity.x, velocity.z);
    const verticalSpeed = deltaSeconds > 0 ? (translation.y - lastLocalCharacterY) / deltaSeconds : 0;
    if (verticalSpeed > JUMP_ANIMATION_VERTICAL_SPEED && !localJumpRising) {
      localCharacterModel?.playJumpAnimation();
    }
    localJumpRising = verticalSpeed > 1.0;
    lastLocalCharacterY = translation.y;
    localCharacterModel?.update(deltaSeconds, speed);

    const cameraFrame = cameraRig.frame({
      x: translation.x,
      y: translation.y + 0.9,
      z: translation.z,
    });
    camera.position.lerp(
      new THREE.Vector3(cameraFrame.position.x, cameraFrame.position.y, cameraFrame.position.z),
      CAMERA_FOLLOW_LERP,
    );
    camera.lookAt(cameraFrame.target.x, cameraFrame.target.y, cameraFrame.target.z);

    marker.position.set(translation.x, 0.04, translation.z);
    marker.rotation.z = frame * 0.02;

    updateAimFeedback(computeCurrentAbilityAim(vec3FromVector(translation)));

    vfxManager.update(deltaSeconds);
    floatingOverlay.update(camera, renderer.domElement.getBoundingClientRect(), deltaSeconds);
    renderNow();
    const localStats = localPlayer.stats();
    const remoteStats = remoteWorld.stats();
    if (window.__DIVE_WEB_ACTIVE_SCENE__ === sceneToken) {
      window.__DIVE_WEB_READY__ = frame > 2;
      window.__DIVE_WEB_SCENE_STATS__ = {
        bundleId: bundle.bundleId,
        colliderCount: physics.colliderCount,
        frame,
        tick: lastSnapshot?.latestTick,
        pendingInputs: lastSnapshot?.inputQueue.pendingCount ?? 0,
        remoteEntities: remoteStats.entityCount,
        reconcileErrEwma: localStats.reconcileErrEwma,
        reconcileErrMax: localStats.reconcileErrMax,
        snapCorrections: localStats.snapCorrections,
        replayedInputs: localStats.replayedInputs,
        lastReplayedSequence: localStats.lastReplayedSequence,
        lastAuthoritativeTick: localStats.lastAuthoritativeTick,
        targetLeadMeters: localStats.targetLeadMeters,
        replayCollisionCount: localStats.replayCollisionCount,
        replayRequestedMeters: localStats.replayRequestedMeters,
        replayCorrectedMeters: localStats.replayCorrectedMeters,
        replayBlockedRatio: localStats.replayBlockedRatio,
        replayPhysicsMs: localStats.replayPhysicsMs,
        kccGrounded: kccStats.grounded,
        kccCollisions: kccStats.collisionCount,
        kccRequestedMeters: kccStats.requestedMeters,
        kccCorrectedMeters: kccStats.correctedMeters,
        kccBlockedRatio: kccStats.blockedRatio,
        kccHorizontalRequestedMeters: kccStats.horizontalRequestedMeters,
        kccHorizontalCorrectedMeters: kccStats.horizontalCorrectedMeters,
        kccHorizontalBlockedRatio: kccStats.horizontalBlockedRatio,
        kccVerticalRequestedMeters: kccStats.verticalRequestedMeters,
        kccVerticalCorrectedMeters: kccStats.verticalCorrectedMeters,
        kccVerticalBlockedRatio: kccStats.verticalBlockedRatio,
        kccVerticalVelocity: kccStats.verticalVelocity,
        kccMoveMs: lastKccMoveMs,
        aimRaycastMs: lastAimRaycastMs,
        extrapolationEvents: remoteStats.extrapolationEvents,
        extrapolationSecsCurrent: remoteStats.extrapolationSecsCurrent,
        extrapolationSecsMax: remoteStats.extrapolationSecsMax,
        snapshotGapEwma: remoteStats.snapshotGapEwma,
        snapshotGapMax: remoteStats.snapshotGapMax,
        remoteIngestMs: lastRemoteIngestMs,
        selectedTarget: targetSelection.getSelected(),
        hoverTarget: targetSelection.getHover(),
        tabCandidates: tabCandidateCount,
        coarsePointer: isCoarsePointer(),
        cameraMode: cameraFrame.mode,
        cameraCaptured: cameraCaptureActive,
        cameraCapturePending: cameraCapturePendingClick,
        cameraYaw: cameraFrame.yaw,
        cameraPitch: cameraFrame.pitch,
        cameraDistance: cameraFrame.distance,
        vfxActiveEffects: vfxManager.activeCount(),
      };
    }
    requestAnimationFrame(animate);
  };

  window.__DIVE_WEB_SAMPLE_CANVAS__ = () => sampleCanvas(renderer, renderNow);
  window.__DIVE_WEB_ACTIVE_SCENE__ = sceneToken;
  cleanups.push(() => {
    if (window.__DIVE_WEB_ACTIVE_SCENE__ === sceneToken) {
      delete window.__DIVE_WEB_READY__;
      delete window.__DIVE_WEB_SAMPLE_CANVAS__;
      delete window.__DIVE_WEB_AIM_DIAGNOSTICS__;
      delete window.__DIVE_WEB_SCENE_STATS__;
      delete window.__DIVE_WEB_ACTIVE_SCENE__;
    }
  });

  animate();

  return {
    destroy: runCleanup,
  };
  } catch (error) {
    runCleanup();
    throw error;
  }
}

function meshForCollider(collider: BundleCollider): THREE.Object3D | undefined {
  const shape = collider.shape;
  const material = new THREE.MeshStandardMaterial({
    color: shape.kind === 'cuboid' ? 0x2f4750 : 0x8c6b4f,
    roughness: 0.72,
    metalness: 0.02,
  });
  const wire = new THREE.LineBasicMaterial({ color: 0x9be7d4, transparent: true, opacity: 0.45 });

  let geometry: THREE.BufferGeometry;
  switch (shape.kind) {
    case 'cuboid':
      geometry = new THREE.BoxGeometry(shape.half_x * 2, shape.half_y * 2, shape.half_z * 2);
      break;
    case 'cylinder':
      geometry = new THREE.CylinderGeometry(shape.radius, shape.radius, shape.half_height * 2, 32);
      break;
    case 'heightfield':
      geometry = heightfieldGeometry(shape);
      break;
    case 'tri_mesh':
      geometry = triMeshGeometry(shape.vertices, shape.indices);
      break;
    default: {
      const _exhaustive: never = shape;
      return _exhaustive;
    }
  }

  const mesh = new THREE.Mesh(geometry, material);
  mesh.position.set(collider.position[0] ?? 0, collider.position[1] ?? 0, collider.position[2] ?? 0);
  mesh.quaternion.set(collider.rotation[0] ?? 0, collider.rotation[1] ?? 0, collider.rotation[2] ?? 0, collider.rotation[3] ?? 1);
  mesh.castShadow = false;
  mesh.receiveShadow = true;

  const edges = new THREE.LineSegments(new THREE.EdgesGeometry(geometry), wire);
  mesh.add(edges);
  return mesh;
}

function setLineEndpoints(geometry: THREE.BufferGeometry, start: Vec3, end: Vec3): void {
  const position = geometry.getAttribute('position') as THREE.BufferAttribute;
  position.setXYZ(0, start.x, start.y, start.z);
  position.setXYZ(1, end.x, end.y, end.z);
  position.needsUpdate = true;
  geometry.computeBoundingSphere();
}

function previewRadius(shape: PreviewShape): number {
  switch (shape.kind) {
    case 'sphere':
      return Math.max(0.9, Math.min(6, shape.radius));
    case 'capsule':
      return Math.max(0.9, Math.min(4, shape.radius + shape.halfHeight));
    case 'none':
      return 0.9;
  }
}

function vec3FromVector(value: { x: number; y: number; z: number }): Vec3 {
  return {
    x: value.x,
    y: value.y,
    z: value.z,
  };
}

function heightfieldGeometry(shape: Extract<BundleCollider['shape'], { kind: 'heightfield' }>): THREE.BufferGeometry {
  const mesh = heightfieldToTriMesh(shape);
  return mesh ? triMeshGeometry(mesh.vertices, mesh.indices) : new THREE.BufferGeometry();
}

function triMeshGeometry(vertices: readonly number[], indices: readonly number[]): THREE.BufferGeometry {
  const geometry = new THREE.BufferGeometry();
  geometry.setAttribute('position', new THREE.Float32BufferAttribute(vertices, 3));
  geometry.setIndex(Array.from(indices));
  geometry.computeVertexNormals();
  return geometry;
}

function inputAllowed(snapshot: StdbSnapshot | undefined, options: SceneOptions): boolean {
  return reducerInputAllowed(snapshot, options) && (options.inputMode?.() ?? 'gameplay') === 'gameplay';
}

function reducerInputAllowed(snapshot: StdbSnapshot | undefined, options: SceneOptions): boolean {
  return (options.inputEnabled?.() ?? true) && snapshot?.state === 'ready' && snapshot.ownDeath === undefined;
}

function sampleCanvas(renderer: THREE.WebGLRenderer, renderNow: () => void): number[] {
  renderNow();
  const probe = document.createElement('canvas');
  probe.width = 8;
  probe.height = 8;
  const ctx = probe.getContext('2d', { willReadFrequently: true });
  const max = [0, 0, 0, 0];
  if (ctx) {
    ctx.drawImage(renderer.domElement, 0, 0, probe.width, probe.height);
    const data = ctx.getImageData(0, 0, probe.width, probe.height).data;
    for (let idx = 0; idx < data.length; idx += 4) {
      max[0] = Math.max(max[0], data[idx] ?? 0);
      max[1] = Math.max(max[1], data[idx + 1] ?? 0);
      max[2] = Math.max(max[2], data[idx + 2] ?? 0);
      max[3] = Math.max(max[3], data[idx + 3] ?? 0);
    }
  }
  return max;
}


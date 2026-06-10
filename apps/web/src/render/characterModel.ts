import * as THREE from 'three';
import * as SkeletonUtils from 'three/examples/jsm/utils/SkeletonUtils.js';

export type CharacterOverlayAnimationRequest = {
  /** Exact or semantic clip names to try first, case-insensitive. */
  clipNames?: readonly string[];
  /** Keyword fallbacks matched against clip names, case-insensitive. */
  fallbackKeywords?: readonly string[];
  /** Desired overlay duration in milliseconds. */
  durationMs: number;
};

export class CharacterModelLoader {
  private gltfPromise: Promise<THREE.Group> | undefined;

  async load(): Promise<THREE.Group> {
    if (this.gltfPromise) {
      return this.gltfPromise;
    }

    this.gltfPromise = (async () => {
      const { GLTFLoader } = await import('three/examples/jsm/loaders/GLTFLoader.js');
      const loader = new GLTFLoader();
      const gltf = await loader.loadAsync('/models/character.glb');
      const scene = gltf.scene;

      scene.traverse((node) => {
        if (node instanceof THREE.Mesh) {
          node.castShadow = true;
          node.receiveShadow = true;
          if (node.material instanceof THREE.MeshStandardMaterial) {
            // Tweak material slightly for visibility if needed
            node.material.roughness = 0.6;
            node.material.metalness = 0.1;
          }
        }
      });

      // Character scale normalization: scale the model so it is ~1.8m tall.
      // Often mixamo models are 100x bigger (cm vs m) or need a slight tweak.
      const box = new THREE.Box3().setFromObject(scene);
      const height = box.max.y - box.min.y;
      if (height > 0) {
        const targetHeight = 0.8;
        const scale = targetHeight / height;
        scene.scale.setScalar(scale);
      }

      scene.userData.animations = gltf.animations;
      return scene;
    })();

    return this.gltfPromise;
  }
}

export class CharacterModel {
  public group: THREE.Object3D;
  private mixer: THREE.AnimationMixer;
  private idleAction?: THREE.AnimationAction;
  private walkAction?: THREE.AnimationAction;
  private runAction?: THREE.AnimationAction;
  private attackAction?: THREE.AnimationAction;
  private jumpAction?: THREE.AnimationAction;
  private activeOverlayAction?: THREE.AnimationAction;
  private overlayTimeout?: NodeJS.Timeout;
  private readonly clipActions = new Map<string, THREE.AnimationAction>();

  constructor(prototypeScene: THREE.Group) {
    // SkeletonUtils is required to properly clone skinned meshes
    this.group = SkeletonUtils.clone(prototypeScene);
    
    // Rotate 180 degrees since the spider model is backwards
    this.group.rotation.y = Math.PI;

    // Deep clone materials so highlights on one spider don't affect others
    this.group.traverse((node) => {
      if (node instanceof THREE.Mesh) {
        if (node.material) {
          if (Array.isArray(node.material)) {
            node.material = node.material.map(m => m.clone());
          } else {
            node.material = node.material.clone();
          }
        }
      }
    });

    this.mixer = new THREE.AnimationMixer(this.group);

    const animations = prototypeScene.userData.animations as THREE.AnimationClip[];
    if (animations && animations.length > 0) {
      for (const clip of animations) {
        this.clipActions.set(clip.name.toLowerCase(), this.mixer.clipAction(clip));
      }
      const findAction = (keyword: string) => this.findActionByKeyword(keyword);

      this.idleAction = findAction('idle') ?? (animations.length > 0 ? this.mixer.clipAction(animations[0]) : undefined);
      // Prefer forward walk/run if available
      this.walkAction = findAction('walk_forward') ?? findAction('walk') ?? (animations.length > 1 ? this.mixer.clipAction(animations[1]) : undefined);
      this.runAction = findAction('run_forward') ?? findAction('run');
      this.attackAction = findAction('attack');
      this.jumpAction = findAction('jump');

      if (this.idleAction) {
        this.idleAction.play();
        this.idleAction.setEffectiveWeight(1.0);
      }
      if (this.walkAction) {
        this.walkAction.play();
        this.walkAction.setEffectiveWeight(0.0);
      }
      if (this.runAction) {
        this.runAction.play();
        this.runAction.setEffectiveWeight(0.0);
      }
      if (this.attackAction) {
        // Setup attack as an overlay that doesn't loop
        this.attackAction.setLoop(THREE.LoopOnce, 1);
        this.attackAction.clampWhenFinished = true;
      }
    }
  }

  update(deltaSeconds: number, velocityMag: number) {
    // Velocity threshold for walking/running
    // 0 -> Idle, up to 3 -> Walk, > 3 -> Run
    let walkWeight = 0;
    let runWeight = 0;

    if (velocityMag > 3.0 && this.runAction) {
      runWeight = Math.min(1.0, (velocityMag - 3.0) / 3.0);
      walkWeight = 1.0 - runWeight;
    } else {
      walkWeight = Math.min(1.0, velocityMag / 3.0);
    }
    const idleWeight = 1.0 - walkWeight - runWeight;

    // If an overlay (like attack) is playing, we fade out base movement to emphasize it
    let baseMultiplier = 1.0;
    if (this.activeOverlayAction && this.activeOverlayAction.isRunning()) {
      baseMultiplier = 0.2; // Dim the legs/idle heavily so attack takes over
    }

    if (this.idleAction) this.idleAction.setEffectiveWeight(idleWeight * baseMultiplier);
    if (this.walkAction) this.walkAction.setEffectiveWeight(walkWeight * baseMultiplier);
    if (this.runAction) this.runAction.setEffectiveWeight(runWeight * baseMultiplier);

    // Scale the walk/run animation speed based on velocity
    if (this.walkAction && walkWeight > 0) {
      this.walkAction.setEffectiveTimeScale(Math.max(0.5, velocityMag / 3.0));
    }
    if (this.runAction && runWeight > 0) {
      this.runAction.setEffectiveTimeScale(Math.max(0.8, velocityMag / 6.0));
    }

    this.mixer.update(deltaSeconds);
  }

  /**
   * Play the attack overlay animation for roughly durationMs. If the attack
   * overlay is already running we leave it alone: this lets a fresh local
   * prediction continue uninterrupted when the authoritative CastStart event
   * confirms it ~1 RTT later, so the model does not visibly restart.
   */
  playSkillAnimation(durationMs: number) {
    this.playOverlayAnimation({ fallbackKeywords: ['attack'], durationMs });
  }

  playOverlayAnimation(request: CharacterOverlayAnimationRequest) {
    const action = this.resolveOverlayAction(request);
    if (!action) return;
    if (this.activeOverlayAction === action && action.isRunning()) return;

    if (this.activeOverlayAction) {
      this.activeOverlayAction.stop();
    }
    if (this.overlayTimeout) {
      clearTimeout(this.overlayTimeout);
    }

    this.activeOverlayAction = action;
    action.reset();
    action.setLoop(THREE.LoopOnce, 1);
    action.clampWhenFinished = true;
    
    // Calculate time scale so the animation fits exactly into the skill duration
    const clipDuration = action.getClip().duration;
    const targetDurationSecs = Math.max(0.05, request.durationMs / 1000.0);
    const timeScale = Math.max(0.5, clipDuration / targetDurationSecs);
    
    action.setEffectiveTimeScale(timeScale);
    action.setEffectiveWeight(1.0);
    action.play();
    action.fadeIn(0.1);

    this.overlayTimeout = setTimeout(() => {
      if (this.activeOverlayAction === action) {
        action.fadeOut(0.2);
        this.activeOverlayAction = undefined;
      }
    }, request.durationMs);
  }

  cancelSkillAnimation() {
    if (this.activeOverlayAction) {
      this.activeOverlayAction.fadeOut(0.2);
      this.activeOverlayAction = undefined;
    }
    if (this.overlayTimeout) {
      clearTimeout(this.overlayTimeout);
      this.overlayTimeout = undefined;
    }
  }

  playJumpAnimation() {
    if (!this.jumpAction) return;

    if (this.activeOverlayAction === this.jumpAction) return; // Already jumping
    
    if (this.activeOverlayAction) {
      this.activeOverlayAction.stop();
    }
    if (this.overlayTimeout) {
      clearTimeout(this.overlayTimeout);
    }

    this.activeOverlayAction = this.jumpAction;
    this.jumpAction.reset();
    this.jumpAction.setLoop(THREE.LoopOnce, 1);
    this.jumpAction.clampWhenFinished = true;
    this.jumpAction.setEffectiveTimeScale(1.0);
    this.jumpAction.setEffectiveWeight(1.0);
    this.jumpAction.play();
    this.jumpAction.fadeIn(0.1);

    // Jump usually takes ~0.5s to 1s
    this.overlayTimeout = setTimeout(() => {
      this.jumpAction?.fadeOut(0.2);
      if (this.activeOverlayAction === this.jumpAction) {
        this.activeOverlayAction = undefined;
      }
    }, 600); // generic duration, could be tied to grounded state instead
  }

  dispose() {
    if (this.overlayTimeout) clearTimeout(this.overlayTimeout);
    this.mixer.stopAllAction();
    this.mixer.uncacheRoot(this.group);
  }

  private resolveOverlayAction(request: CharacterOverlayAnimationRequest): THREE.AnimationAction | undefined {
    for (const name of request.clipNames ?? []) {
      const exact = this.clipActions.get(name.toLowerCase());
      if (exact) return exact;
      const keyword = this.findActionByKeyword(name);
      if (keyword) return keyword;
    }
    for (const keyword of request.fallbackKeywords ?? []) {
      const action = this.findActionByKeyword(keyword);
      if (action) return action;
    }
    return undefined;
  }

  private findActionByKeyword(keyword: string): THREE.AnimationAction | undefined {
    const normalized = keyword.toLowerCase();
    for (const [clipName, action] of this.clipActions) {
      if (clipName.includes(normalized)) return action;
    }
    return undefined;
  }
}

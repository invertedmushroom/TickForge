import type { UiRuntime } from './ui/runtime';

export {};

declare global {
  interface Window {
    __DIVE_WEB_UI_RUNTIME__?: UiRuntime;
    __DIVE_WEB_READY__?: boolean;
    __DIVE_WEB_SAMPLE_CANVAS__?: () => number[];
    __DIVE_WEB_ACTIVE_SCENE__?: object;
    __DIVE_WEB_AIM_DIAGNOSTICS__?: {
      slot: number;
      focusedSlot?: number;
      armedSlot?: number;
      abilityId: number;
      abilityName: string;
      targeting: string;
      hasGround: boolean;
      validGround: boolean;
      softTarget?: bigint;
      aimPoint?: { x: number; y: number; z: number };
      aimDirection?: { x: number; y: number; z: number };
      canvasX?: number;
      canvasY?: number;
      lastSubmittedAbilityId?: number;
      lastReleasedAbilityId?: number;
      lastActionInputGesture?: string;
      lastSubmitSuppressed?: string;
    };
    __DIVE_WEB_SCENE_STATS__?: {
      bundleId: string;
      colliderCount: number;
      frame: number;
      tick?: bigint;
      pendingInputs?: number;
      remoteEntities?: number;
      reconcileErrEwma?: number;
      reconcileErrMax?: number;
      snapCorrections?: number;
      replayedInputs?: number;
      lastReplayedSequence?: bigint;
      lastAuthoritativeTick?: bigint;
      targetLeadMeters?: number;
      replayCollisionCount?: number;
      replayRequestedMeters?: number;
      replayCorrectedMeters?: number;
      replayBlockedRatio?: number;
      replayPhysicsMs?: number;
      kccGrounded?: boolean;
      kccCollisions?: number;
      kccRequestedMeters?: number;
      kccCorrectedMeters?: number;
      kccBlockedRatio?: number;
      kccHorizontalRequestedMeters?: number;
      kccHorizontalCorrectedMeters?: number;
      kccHorizontalBlockedRatio?: number;
      kccVerticalRequestedMeters?: number;
      kccVerticalCorrectedMeters?: number;
      kccVerticalBlockedRatio?: number;
      kccVerticalVelocity?: number;
      kccMoveMs?: number;
      aimRaycastMs?: number;
      extrapolationEvents?: number;
      extrapolationSecsCurrent?: number;
      extrapolationSecsMax?: number;
      snapshotGapEwma?: number;
      snapshotGapMax?: number;
      remoteIngestMs?: number;
      selectedTarget?: bigint;
      hoverTarget?: bigint;
      tabCandidates?: number;
      coarsePointer?: boolean;
      cameraMode?: 'follow' | 'detached';
      cameraCaptured?: boolean;
      cameraCapturePending?: boolean;
      cameraYaw?: number;
      cameraPitch?: number;
      cameraDistance?: number;
      vfxActiveEffects?: number;
    };
  }
}

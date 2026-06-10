export type CameraRigMode = 'follow' | 'detached';

export type CameraRigVec3 = {
  x: number;
  y: number;
  z: number;
};

export type CameraRigFrame = {
  position: CameraRigVec3;
  target: CameraRigVec3;
  yaw: number;
  pitch: number;
  distance: number;
  mode: CameraRigMode;
};

export type CameraRigConfig = {
  minPitch: number;
  maxPitch: number;
  minDistance: number;
  maxDistance: number;
  yawSensitivity: number;
  pitchSensitivity: number;
  zoomSensitivity: number;
};

export const DEFAULT_CAMERA_RIG_CONFIG: Readonly<CameraRigConfig> = Object.freeze({
  minPitch: 0.18,
  maxPitch: 1.05,
  minDistance: 6,
  maxDistance: 34,
  yawSensitivity: 0.0026,
  pitchSensitivity: 0.0022,
  zoomSensitivity: 0.0015,
});

export const DEFAULT_CAMERA_YAW = Math.atan2(14, 16);
export const DEFAULT_CAMERA_PITCH = Math.atan2(9, Math.hypot(14, 16));
export const DEFAULT_CAMERA_DISTANCE = Math.hypot(14, 9, 16);

export class OrbitCameraRig {
  private readonly config: CameraRigConfig;
  private yaw = DEFAULT_CAMERA_YAW;
  private pitch = DEFAULT_CAMERA_PITCH;
  private distance = DEFAULT_CAMERA_DISTANCE;
  private mode: CameraRigMode = 'follow';
  private detachedTarget: CameraRigVec3 | undefined;

  constructor(config: Partial<CameraRigConfig> = {}) {
    this.config = { ...DEFAULT_CAMERA_RIG_CONFIG, ...config };
    this.pitch = clamp(this.pitch, this.config.minPitch, this.config.maxPitch);
    this.distance = clamp(this.distance, this.config.minDistance, this.config.maxDistance);
  }

  rotate(deltaX: number, deltaY: number, sensitivityScale = 1): void {
    if (!Number.isFinite(deltaX) || !Number.isFinite(deltaY)) {
      return;
    }
    this.yaw = wrapRadians(this.yaw + deltaX * this.config.yawSensitivity * sensitivityScale);
    this.pitch = clamp(
      this.pitch - deltaY * this.config.pitchSensitivity * sensitivityScale,
      this.config.minPitch,
      this.config.maxPitch,
    );
  }

  zoom(deltaY: number): void {
    if (!Number.isFinite(deltaY)) {
      return;
    }
    this.distance = clamp(
      this.distance * (1 + deltaY * this.config.zoomSensitivity),
      this.config.minDistance,
      this.config.maxDistance,
    );
  }

  detach(target: CameraRigVec3): void {
    this.mode = 'detached';
    this.detachedTarget = cloneVec3(target);
  }

  attach(): void {
    this.mode = 'follow';
    this.detachedTarget = undefined;
  }

  toggleDetached(currentTarget: CameraRigVec3): CameraRigMode {
    if (this.mode === 'detached') {
      this.attach();
    } else {
      this.detach(currentTarget);
    }
    return this.mode;
  }

  frame(followTarget: CameraRigVec3): CameraRigFrame {
    const target = this.mode === 'detached' && this.detachedTarget
      ? this.detachedTarget
      : followTarget;
    const offset = orbitOffset(this.yaw, this.pitch, this.distance);
    return {
      position: {
        x: target.x + offset.x,
        y: target.y + offset.y,
        z: target.z + offset.z,
      },
      target: cloneVec3(target),
      yaw: this.yaw,
      pitch: this.pitch,
      distance: this.distance,
      mode: this.mode,
    };
  }

  snapshot(): Pick<CameraRigFrame, 'yaw' | 'pitch' | 'distance' | 'mode'> {
    return {
      yaw: this.yaw,
      pitch: this.pitch,
      distance: this.distance,
      mode: this.mode,
    };
  }
}

export function rotateMovementByCameraYaw(inputX: number, inputZ: number, cameraYaw: number): { x: number; z: number } {
  if (!Number.isFinite(inputX) || !Number.isFinite(inputZ) || !Number.isFinite(cameraYaw)) {
    return { x: 0, z: 0 };
  }
  const sin = Math.sin(cameraYaw);
  const cos = Math.cos(cameraYaw);
  return {
    x: inputX * cos + inputZ * sin,
    z: -inputX * sin + inputZ * cos,
  };
}

function orbitOffset(yaw: number, pitch: number, distance: number): CameraRigVec3 {
  const horizontal = Math.cos(pitch) * distance;
  return {
    x: Math.sin(yaw) * horizontal,
    y: Math.sin(pitch) * distance,
    z: Math.cos(yaw) * horizontal,
  };
}

function cloneVec3(value: CameraRigVec3): CameraRigVec3 {
  return { x: value.x, y: value.y, z: value.z };
}

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function wrapRadians(value: number): number {
  const twoPi = Math.PI * 2;
  return ((value + Math.PI) % twoPi + twoPi) % twoPi - Math.PI;
}

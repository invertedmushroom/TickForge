export function normalizeDirection(x: number, z: number): { x: number; z: number } {
  const length = Math.hypot(x, z);
  if (length < 0.001) {
    return { x: 0, z: 0 };
  }
  return { x: x / length, z: z / length };
}

export type HeightfieldShape = {
  nrows: number;
  ncols: number;
  scale_x: number;
  scale_y: number;
  scale_z: number;
  heights: readonly number[];
};

export type TriMeshData = {
  vertices: number[];
  indices: number[];
};

export function heightfieldToTriMesh(shape: HeightfieldShape): TriMeshData | undefined {
  const rows = Math.trunc(shape.nrows);
  const cols = Math.trunc(shape.ncols);
  if (rows < 2 || cols < 2 || shape.heights.length !== rows * cols) {
    return undefined;
  }

  const vertices: number[] = [];
  const indices: number[] = [];
  const stepX = shape.scale_x / (rows - 1);
  const stepZ = shape.scale_z / (cols - 1);

  for (let row = 0; row < rows; row += 1) {
    for (let col = 0; col < cols; col += 1) {
      const x = -shape.scale_x * 0.5 + row * stepX;
      const z = -shape.scale_z * 0.5 + col * stepZ;
      const y = (shape.heights[row + col * rows] ?? 0) * shape.scale_y;
      vertices.push(x, y, z);
    }
  }

  const index = (row: number, col: number) => row * cols + col;
  for (let row = 0; row < rows - 1; row += 1) {
    for (let col = 0; col < cols - 1; col += 1) {
      const a = index(row, col);
      const b = index(row, col + 1);
      const c = index(row + 1, col);
      const d = index(row + 1, col + 1);
      indices.push(a, b, c, c, b, d);
    }
  }

  return { vertices, indices };
}
